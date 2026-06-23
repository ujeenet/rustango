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
//! For strict RFC layout the `.well-known/*` documents belong at the origin
//! root; the handlers are `pub` so an app can also mount them there.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
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
        "token_endpoint_auth_methods_supported": ["client_secret_post"],
        "response_types_supported": [],
        "scopes_supported": [],
    })
}

/// Best-effort origin (`scheme://host`) from request headers. Honors
/// `X-Forwarded-Proto`; defaults to `http` for localhost, `https` otherwise.
fn origin(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if host.starts_with("localhost") || host.starts_with("127.") {
                "http".into()
            } else {
                "https".into()
            }
        });
    format!("{scheme}://{host}")
}

/// `GET {prefix}/.well-known/oauth-protected-resource`
pub(crate) async fn well_known_protected_resource(headers: HeaderMap) -> Response {
    let base = origin(&headers);
    Json(protected_resource_metadata(
        &base,
        &format!("{base}/.well-known/oauth-authorization-server"),
    ))
    .into_response()
}

/// `GET {prefix}/.well-known/oauth-authorization-server`
pub(crate) async fn well_known_authorization_server(headers: HeaderMap) -> Response {
    let base = origin(&headers);
    Json(authorization_server_metadata(
        &base,
        &format!("{base}/oauth/token"),
    ))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct OAuthTokenForm {
    pub grant_type: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default)]
    #[allow(dead_code)] // accepted for spec-compliance; scope is derived from grants
    pub scope: Option<String>,
}

/// `POST {prefix}/oauth/token` — OAuth 2.1 client-credentials grant. The
/// `client_id` / `client_secret` are the agent's name / secret; the issued
/// access token is the same scoped JWT as the bespoke endpoint.
pub(crate) async fn oauth_token(
    t: Tenant,
    State(state): State<McpState>,
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
    match mint_agent_jwt(
        jwt,
        t.pool(),
        &t.org.slug,
        &form.client_id,
        &form.client_secret,
    )
    .await
    {
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
    }
}
