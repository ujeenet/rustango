//! MCP Authorization-spec interop (epic #1013, follow-up #1088).
//!
//! **Additive** over the locked scoped-JWT model (epic decision #4): it does
//! not replace agent credentials, it makes them discoverable by
//! spec-compliant MCP clients. Three pieces:
//!
//! * **Protected Resource Metadata** (RFC 9728) at
//!   `{prefix}/.well-known/oauth-protected-resource` — points clients at the
//!   authorization server.
//! * **Authorization Server Metadata** (RFC 8414) at
//!   `{prefix}/.well-known/oauth-authorization-server` — advertises the
//!   `client_credentials` token endpoint.
//! * **OAuth 2.1 client-credentials** at `{prefix}/oauth/token` — the agent
//!   exchanges `client_id`/`client_secret` (its name/secret) for the same
//!   scoped JWT the bespoke `/token` issues.
//!
//! The advertised URLs track the actual mount prefix via the request's
//! [`axum::extract::OriginalUri`] (#1094), so they stay correct whether the
//! MCP router is nested under `/mcp`, `/api/mcp`, or the origin root. Apps that
//! prefer the strict-RFC origin-root layout can additionally re-serve these
//! `.well-known/*` documents at `/` themselves.

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

/// `GET {prefix}/.well-known/oauth-protected-resource`. URLs track the real
/// mount prefix via [`OriginalUri`] (#1094): the `resource` is `{origin}{prefix}`
/// and the authorization server points back under the same prefix.
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

/// `GET {prefix}/.well-known/oauth-authorization-server`. URLs track the real
/// mount prefix via [`OriginalUri`] (#1094).
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
    // Optional in the body: RFC 6749 §2.3.1 allows the client to authenticate
    // via HTTP Basic instead (`client_secret_basic`). Required if Basic is absent.
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted for spec-compliance; scope is derived from grants
    pub scope: Option<String>,
}

/// Decode an `Authorization: Basic base64(client_id:client_secret)` header
/// (RFC 6749 §2.3.1 / RFC 7617). `None` if absent or malformed.
fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = raw.strip_prefix("Basic ")?.trim();
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let creds = String::from_utf8(decoded).ok()?;
    let (id, secret) = creds.split_once(':')?;
    Some((id.to_owned(), secret.to_owned()))
}

/// `POST {prefix}/oauth/token` — OAuth 2.1 client-credentials grant. The
/// `client_id` / `client_secret` are the agent's name / secret, supplied either
/// via HTTP Basic (`client_secret_basic`, preferred) or form fields
/// (`client_secret_post`). The issued access token is the same scoped JWT as
/// the bespoke endpoint.
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
    // HTTP Basic wins over body params (RFC 6749 §2.3.1).
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

/// RFC 6749 §5.2 OAuth error response.
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
        // Both client-auth methods are advertised (#1099: Basic + post).
        let methods = asm["token_endpoint_auth_methods_supported"]
            .as_array()
            .unwrap();
        assert!(methods.iter().any(|m| m == "client_secret_basic"));
        assert!(methods.iter().any(|m| m == "client_secret_post"));
    }

    #[test]
    fn basic_auth_decodes_client_credentials() {
        let mut h = HeaderMap::new();
        // base64("bot:s3cret") = "Ym90OnMzY3JldA=="
        let b64 = base64::engine::general_purpose::STANDARD.encode("bot:s3cret");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {b64}").parse().unwrap(),
        );
        assert_eq!(
            basic_auth(&h),
            Some(("bot".to_owned(), "s3cret".to_owned()))
        );

        // Secret may itself contain ':' — only the first split counts.
        let b64 = base64::engine::general_purpose::STANDARD.encode("bot:a:b");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {b64}").parse().unwrap(),
        );
        assert_eq!(basic_auth(&h), Some(("bot".to_owned(), "a:b".to_owned())));

        // Missing / non-Basic → None.
        assert_eq!(basic_auth(&HeaderMap::new()), None);
    }
}
