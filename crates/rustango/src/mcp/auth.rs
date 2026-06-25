//! Agent authentication (epic #1013, Slice 2 / #1015).
//!
//! Two layers:
//!
//! * **Pure token logic** — [`issue_agent_token`] / [`verify_agent_token`]
//!   wrap [`crate::tenancy::jwt_lifecycle::JwtLifecycle`] with MCP's custom
//!   claims (`kind:"agent"`, `tenant`, `skills`, `tools`). `tenant` is
//!   reserved-claim-safe; note `typ` is reserved by `JwtLifecycle`
//!   (access/refresh), so the agent marker rides under `kind`. These are
//!   plain functions, unit-tested without HTTP.
//! * **HTTP handlers** — [`agent_token`] (the client-credentials
//!   `{name, secret}` → scoped-JWT endpoint) and [`post_authed`] (the
//!   JSON-RPC endpoint guarded by a tenant-pinned agent JWT). Both resolve
//!   the request tenant via the [`Tenant`] extractor and operate on that
//!   tenant's pool — a token minted for tenant A is refused on tenant B.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::extractors::Tenant;
use crate::tenancy::jwt_lifecycle::{JwtIssueError, JwtLifecycle};

use super::router::McpState;
use super::transport::handle_message;

/// Custom-claim key carrying the principal kind; value [`KIND_AGENT`].
pub const CLAIM_KIND: &str = "kind";
/// Value of [`CLAIM_KIND`] for agent tokens.
pub const KIND_AGENT: &str = "agent";
/// Custom-claim key pinning the token to one tenant slug.
pub const CLAIM_TENANT: &str = "tenant";
/// Custom-claim key carrying the agent's granted skill codenames (Slice 4).
pub const CLAIM_SKILLS: &str = "skills";
/// Custom-claim key carrying the agent's flattened tool set (Slice 4).
pub const CLAIM_TOOLS: &str = "tools";

/// A verified agent principal, resolved from a tenant-pinned access token.
#[derive(Debug, Clone)]
pub struct McpAgent {
    /// `rustango_agents.id` (the JWT `sub`).
    pub agent_id: i64,
    /// Tenant slug the token is pinned to.
    pub tenant: String,
    /// Granted skill codenames (empty until Slice 4 fills the claim).
    pub skills: Vec<String>,
    /// Flattened allowed tool set (empty until Slice 4).
    pub tools: Vec<String>,
    /// Unique token id — the revocation handle.
    pub jti: String,
}

/// Issue a short-lived access token for `agent_id`, pinned to `tenant` and
/// carrying its skill/tool grants.
///
/// # Errors
/// [`JwtIssueError`] only if a custom claim collides with a reserved name
/// (it won't here — `kind`/`tenant`/`skills`/`tools` are all non-reserved).
pub fn issue_agent_token(
    jwt: &JwtLifecycle,
    agent_id: i64,
    tenant: &str,
    skills: &[String],
    tools: &[String],
) -> Result<String, JwtIssueError> {
    let mut custom = serde_json::Map::new();
    custom.insert(CLAIM_KIND.into(), json!(KIND_AGENT));
    custom.insert(CLAIM_TENANT.into(), json!(tenant));
    custom.insert(CLAIM_SKILLS.into(), json!(skills));
    custom.insert(CLAIM_TOOLS.into(), json!(tools));
    jwt.issue_access_with(agent_id, custom)
}

/// Verify an agent access token and pin it to `expected_tenant`. Returns
/// the resolved [`McpAgent`], or `None` for any failure — bad signature,
/// expired, revoked JTI, not an agent token, or wrong tenant. Fail-closed.
#[must_use]
pub fn verify_agent_token(
    jwt: &JwtLifecycle,
    token: &str,
    expected_tenant: &str,
) -> Option<McpAgent> {
    let claims = jwt.verify_access(token)?;
    if claims.get_custom::<String>(CLAIM_KIND).as_deref() != Some(KIND_AGENT) {
        return None;
    }
    let tenant = claims.get_custom::<String>(CLAIM_TENANT)?;
    if tenant != expected_tenant {
        return None;
    }
    Some(McpAgent {
        agent_id: claims.sub,
        tenant,
        skills: claims
            .get_custom::<Vec<String>>(CLAIM_SKILLS)
            .unwrap_or_default(),
        tools: claims
            .get_custom::<Vec<String>>(CLAIM_TOOLS)
            .unwrap_or_default(),
        jti: claims.jti,
    })
}

// --------------------------------------------------------------- HTTP layer

#[derive(Debug, Deserialize)]
pub(crate) struct AgentTokenInput {
    pub name: String,
    pub secret: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AgentTokenOutput {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
}

/// Outcome of [`mint_agent_jwt`] — a scoped token + its granted scope, or a
/// typed failure both token endpoints render in their own shape.
pub(crate) struct MintedToken {
    pub token: String,
    pub expires_in: i64,
    /// Space-delimited granted skills (OAuth `scope`).
    pub scope: String,
}

pub(crate) enum MintError {
    /// Bad agent name/secret.
    Unauthorized,
    /// Server-side failure (DB / issuance).
    Internal,
}

/// Shared minting path: authenticate `{name, secret}` against the tenant,
/// resolve grants, and issue a tenant-pinned scoped JWT. Used by both the
/// bespoke JSON `/token` and the OAuth 2.1 client-credentials `/oauth/token`.
pub(crate) async fn mint_agent_jwt(
    jwt: &JwtLifecycle,
    pool: &crate::sql::Pool,
    slug: &str,
    name: &str,
    secret: &str,
) -> Result<MintedToken, MintError> {
    let agent = match crate::tenancy::authenticate_agent_pool(pool, name, secret).await {
        Ok(Some(a)) => a,
        Ok(None) => return Err(MintError::Unauthorized),
        Err(e) => {
            tracing::warn!(error = %e, "mcp agent authentication failed");
            return Err(MintError::Internal);
        }
    };
    let agent_id = agent.id.get().copied().unwrap_or_default();
    let (skills, tools) = crate::tenancy::resolve_agent_grants_pool(pool, agent_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "mcp grant resolution failed");
            MintError::Internal
        })?;
    let token = issue_agent_token(jwt, agent_id, slug, &skills, &tools).map_err(|e| {
        tracing::error!(error = %e, "mcp token issuance failed");
        MintError::Internal
    })?;
    Ok(MintedToken {
        token,
        expires_in: jwt.access_ttl_secs,
        scope: skills.join(" "),
    })
}

/// `POST {prefix}/token` — exchange `{name, secret}` for a scoped agent
/// JWT pinned to the resolved request tenant. Client-credentials style.
pub(crate) async fn agent_token(
    t: Tenant,
    State(state): State<McpState>,
    Json(input): Json<AgentTokenInput>,
) -> Response {
    let Some(jwt) = state.jwt.as_ref() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "mcp auth not configured").into_response();
    };
    match mint_agent_jwt(jwt, t.pool(), &t.org.slug, &input.name, &input.secret).await {
        Ok(m) => Json(AgentTokenOutput {
            access_token: m.token,
            token_type: "Bearer",
            expires_in: m.expires_in,
        })
        .into_response(),
        Err(MintError::Unauthorized) => {
            (StatusCode::UNAUTHORIZED, "invalid agent credentials").into_response()
        }
        Err(MintError::Internal) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "token issuance failed").into_response()
        }
    }
}

/// `POST {prefix}` (authed) — require a tenant-pinned agent JWT, then
/// dispatch the JSON-RPC message. A token whose `tenant` claim ≠ the
/// resolved request tenant (or a revoked / expired one) is refused.
pub(crate) async fn post_authed(
    t: Tenant,
    State(state): State<McpState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(jwt) = state.jwt.as_ref() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "mcp auth not configured").into_response();
    };
    let Some(token) = bearer(&headers) else {
        return unauthorized(&headers, &uri);
    };
    let Some(agent) = verify_agent_token(jwt, token, &t.org.slug) else {
        return unauthorized(&headers, &uri);
    };
    // Agent verified + tenant-pinned: hand the tools layer the resolved
    // tenant pool + principal so `tools/call` runs against the right tenant.
    let ctx = super::tools::McpContext {
        pool: t.pool().clone(),
        agent,
        // Progress + cancellation are wired per-call by `call_tool_with`.
        progress: super::progress::ProgressReporter::disabled(),
        cancel: super::progress::CancelToken::never(),
    };
    handle_message(&state, &body, Some(ctx)).await
}

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// Best-effort request origin (`scheme://host`) from headers. Honors
/// `X-Forwarded-Proto`; defaults to `http` for localhost, `https` otherwise.
/// Shared by [`unauthorized`] and the OAuth metadata handlers.
pub(crate) fn origin(headers: &HeaderMap) -> String {
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

/// Public base URL the MCP surface is actually mounted at, derived from the
/// request origin + the *original* (pre-nest) request path with `strip_suffix`
/// removed and any trailing slash trimmed. This makes RFC-9728 / RFC-8414 URLs
/// track the real `.nest(prefix)` mount instead of assuming origin-root (#1094).
///
/// E.g. origin `https://h`, original path `/mcp/.well-known/oauth-protected-resource`,
/// `strip_suffix = "/.well-known/oauth-protected-resource"` → `https://h/mcp`.
/// For the JSON-RPC endpoint itself the path *is* the prefix, so pass `""`.
pub(crate) fn mount_base(
    headers: &HeaderMap,
    original: &axum::http::Uri,
    strip_suffix: &str,
) -> String {
    let path = original.path();
    let prefix = path
        .strip_suffix(strip_suffix)
        .unwrap_or(path)
        .trim_end_matches('/');
    format!("{}{}", origin(headers), prefix)
}

/// `401` with an RFC 9728 `WWW-Authenticate: Bearer resource_metadata=…`
/// challenge so spec-compliant MCP clients can discover the auth server
/// (#1088). The metadata URL tracks the actual mount prefix via the request's
/// [`OriginalUri`] (#1094), not the origin root.
pub(crate) fn unauthorized(headers: &HeaderMap, original: &axum::http::Uri) -> Response {
    let base = mount_base(headers, original, "");
    let challenge =
        format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#);
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        "missing or invalid agent token",
    )
        .into_response()
}

/// Build the default [`JwtLifecycle`] for the MCP auth router — HMAC key
/// from `RUSTANGO_SESSION_SECRET` (shared with the rest of the framework),
/// falling back to a per-process random key with a warning so dev still
/// works but operators are nudged to set a stable secret.
pub(crate) fn default_jwt() -> Arc<JwtLifecycle> {
    Arc::new(JwtLifecycle::new(jwt_secret()))
}

/// HMAC key for MCP agent tokens — `RUSTANGO_SESSION_SECRET` (shared with
/// the rest of the framework), falling back to a per-process random key
/// with a warning so dev works but operators are nudged to set a stable
/// secret. `manage check --deploy` flags the missing/short secret.
pub(crate) fn jwt_secret() -> Vec<u8> {
    std::env::var("RUSTANGO_SESSION_SECRET")
        .ok()
        .map(String::into_bytes)
        .filter(|s| s.len() >= 32)
        .unwrap_or_else(|| {
            tracing::warn!(
                "RUSTANGO_SESSION_SECRET unset or <32 bytes; MCP agent tokens \
                 are signed with an ephemeral per-process key (tokens won't \
                 survive a restart and won't verify across instances)"
            );
            use rand::RngCore;
            let mut key = vec![0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut key);
            key
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn headers(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, host.parse().unwrap());
        h
    }

    #[test]
    fn mount_base_tracks_the_nest_prefix() {
        let h = headers("app.example");
        // JSON-RPC endpoint: the path *is* the prefix (strip nothing).
        let uri: Uri = "/mcp".parse().unwrap();
        assert_eq!(mount_base(&h, &uri, ""), "https://app.example/mcp");
        // Well-known doc: strip the suffix to recover the prefix.
        let uri: Uri = "/api/mcp/.well-known/oauth-protected-resource"
            .parse()
            .unwrap();
        assert_eq!(
            mount_base(&h, &uri, "/.well-known/oauth-protected-resource"),
            "https://app.example/api/mcp"
        );
        // Origin-root mount → empty prefix (no trailing slash artifact).
        let uri: Uri = "/.well-known/oauth-protected-resource".parse().unwrap();
        assert_eq!(
            mount_base(&h, &uri, "/.well-known/oauth-protected-resource"),
            "https://app.example"
        );
    }

    #[test]
    fn unauthorized_challenge_points_at_the_mount_prefix() {
        let h = headers("app.example");
        let uri: Uri = "/mcp".parse().unwrap();
        let resp = unauthorized(&h, &uri);
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let challenge = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            challenge,
            r#"Bearer resource_metadata="https://app.example/mcp/.well-known/oauth-protected-resource""#
        );
    }

    #[test]
    fn localhost_origin_uses_http() {
        let h = headers("localhost:8080");
        let uri: Uri = "/mcp".parse().unwrap();
        assert_eq!(mount_base(&h, &uri, ""), "http://localhost:8080/mcp");
    }
}
