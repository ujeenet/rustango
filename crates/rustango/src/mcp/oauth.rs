//! OAuth discovery, so a standards-compliant MCP client can find the
//! agent credentials on its own.
//!
//! This adds to the scoped-JWT model rather than replacing it. Three
//! endpoints:
//!
//! * `{prefix}/.well-known/oauth-protected-resource` (RFC 9728)
//!   points the client at the authorization server.
//! * `{prefix}/.well-known/oauth-authorization-server` (RFC 8414)
//!   advertises the token endpoint.
//! * `{prefix}/oauth/token` trades a `client_id` and
//!   `client_secret`, which are the agent's name and secret, for the
//!   same scoped JWT that `/token` issues.
//!
//! The URLs in those documents follow the real mount prefix, read
//! from the request, so they are right whether the router sits at
//! `/mcp`, `/api/mcp` or the root. If you need the documents at the
//! origin root, as the RFCs prefer, serve them there yourself too.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::extractors::Tenant;

use super::auth::{mint_agent_jwt, MintError};
use super::router::McpState;

/// RFC 9728 Protected Resource Metadata document.
#[must_use]
pub fn protected_resource_metadata(resource: &str, authorization_server: &str) -> Value {
    json!({
        "resource": resource,
        "authorization_servers": [authorization_server],
        "bearer_methods_supported": ["header"],
    })
}

/// RFC 8414 Authorization Server Metadata document.
#[must_use]
pub fn authorization_server_metadata(issuer: &str, token_endpoint: &str) -> Value {
    json!({
        "issuer": issuer,
        "token_endpoint": token_endpoint,
        "grant_types_supported": ["client_credentials"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "response_types_supported": [],
        "scopes_supported": [],
    })
}

/// `GET {prefix}/.well-known/oauth-protected-resource`. The
/// `resource` is the origin plus the real mount prefix, and the
/// authorization server points back under that same prefix.
pub(crate) async fn well_known_protected_resource(
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Response {
    let base = super::auth::mount_base(&headers, &uri, "/.well-known/oauth-protected-resource");
    Json(protected_resource_metadata(
        &base,
        &format!("{base}/.well-known/oauth-authorization-server"),
    ))
    .into_response()
}

/// `GET {prefix}/.well-known/oauth-authorization-server`. The URLs
/// follow the real mount prefix.
pub(crate) async fn well_known_authorization_server(
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Response {
    let base = super::auth::mount_base(&headers, &uri, "/.well-known/oauth-authorization-server");
    Json(authorization_server_metadata(
        &base,
        &format!("{base}/oauth/token"),
    ))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct OAuthTokenForm {
    pub grant_type: String,
    // Optional here, because the client may instead authenticate
    // with HTTP Basic. Required when it does not.
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted for spec-compliance; scope is derived from grants
    pub scope: Option<String>,
}

/// Decode an `Authorization: Basic base64(client_id:client_secret)`
/// header. `None` when it is missing or malformed.
fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = raw.strip_prefix("Basic ")?.trim();
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let creds = String::from_utf8(decoded).ok()?;
    let (id, secret) = creds.split_once(':')?;
    Some((id.to_owned(), secret.to_owned()))
}

/// `POST {prefix}/oauth/token`: the OAuth 2.1 client-credentials
/// grant. The `client_id` and `client_secret` are the agent's name
/// and secret. Send them in an HTTP Basic header, which is
/// preferred, or as form fields. The token it returns is the same
/// scoped JWT as `/token`.
pub(crate) async fn oauth_token(
    t: Tenant,
    State(state): State<McpState>,
    headers: HeaderMap,
    Form(form): Form<OAuthTokenForm>,
) -> Response {
    let Some(jwt) = state.jwt.as_ref() else {
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "auth not configured",
        );
    };
    if form.grant_type != "client_credentials" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only client_credentials is supported",
        );
    }
    // An HTTP Basic header wins over the form fields.
    let creds = basic_auth(&headers).or_else(|| match (form.client_id, form.client_secret) {
        (Some(id), Some(secret)) => Some((id, secret)),
        _ => None,
    });
    let Some((client_id, client_secret)) = creds else {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "missing client credentials (HTTP Basic or client_id/client_secret)",
        );
    };
    match mint_agent_jwt(jwt, t.pool(), &t.org.slug, &client_id, &client_secret).await {
        Ok(m) => Json(json!({
            "access_token": m.token,
            "token_type": "Bearer",
            "expires_in": m.expires_in,
            "scope": m.scope,
        }))
        .into_response(),
        Err(MintError::Unauthorized) => oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "invalid client credentials",
        ),
        Err(MintError::Internal) => oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "token issuance failed",
        ),
    }
}

/// An OAuth error response, in the shape RFC 6749 defines.
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_documents_advertise_client_credentials() {
        let prm = protected_resource_metadata(
            "https://app.example",
            "https://app.example/.well-known/oauth-authorization-server",
        );
        assert_eq!(prm["resource"], "https://app.example");
        assert_eq!(
            prm["authorization_servers"][0],
            "https://app.example/.well-known/oauth-authorization-server"
        );

        let asm =
            authorization_server_metadata("https://app.example", "https://app.example/oauth/token");
        assert_eq!(asm["issuer"], "https://app.example");
        assert_eq!(asm["token_endpoint"], "https://app.example/oauth/token");
        assert_eq!(asm["grant_types_supported"][0], "client_credentials");
        // Both client-auth methods are advertised.
        let methods = asm["token_endpoint_auth_methods_supported"]
            .as_array()
            .unwrap();
        assert!(methods.iter().any(|m| m == "client_secret_basic"));
        assert!(methods.iter().any(|m| m == "client_secret_post"));
    }

    #[test]
    fn basic_auth_decodes_client_credentials() {
        let mut h = HeaderMap::new();
        // The header below is base64 of "bot:s3cret".
        let b64 = base64::engine::general_purpose::STANDARD.encode("bot:s3cret");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {b64}").parse().unwrap(),
        );
        assert_eq!(
            basic_auth(&h),
            Some(("bot".to_owned(), "s3cret".to_owned()))
        );

        // A secret may contain a colon, so only the first one splits.
        let b64 = base64::engine::general_purpose::STANDARD.encode("bot:a:b");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {b64}").parse().unwrap(),
        );
        assert_eq!(basic_auth(&h), Some(("bot".to_owned(), "a:b".to_owned())));

        // A missing header, or one that is not Basic, gives None.
        assert_eq!(basic_auth(&HeaderMap::new()), None);
    }
}
