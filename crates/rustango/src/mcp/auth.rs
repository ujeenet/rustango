//! Agent authentication, in two layers.
//!
//! **Token logic.** [`issue_agent_token`] and [`verify_agent_token`]
//! wrap [`crate::tenancy::jwt_lifecycle::JwtLifecycle`] with the
//! claims MCP needs: `kind`, `tenant`, `skills` and `tools`. The
//! agent marker goes under `kind`, because `JwtLifecycle` reserves
//! `typ` for access and refresh. These are plain functions, testable
//! without HTTP.
//!
//! **HTTP handlers.** [`agent_token`] trades a `{name, secret}` pair
//! for a scoped JWT, and [`post_authed`] is the JSON-RPC endpoint
//! behind such a token. Both resolve the tenant with the [`Tenant`]
//! extractor and work on that tenant's pool, so a token minted for
//! one tenant is refused on another.

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

/// Claim naming what kind of principal this is; see [`KIND_AGENT`].
pub const CLAIM_KIND: &str = "kind";
/// The [`CLAIM_KIND`] value an agent token carries.
pub const KIND_AGENT: &str = "agent";
/// Claim pinning the token to one tenant slug.
pub const CLAIM_TENANT: &str = "tenant";
/// Claim listing the agent's granted skill codenames.
pub const CLAIM_SKILLS: &str = "skills";
/// Claim listing the tools those skills add up to.
pub const CLAIM_TOOLS: &str = "tools";
/// Claim naming the user who owns the key. A machine agent has none.
pub const CLAIM_UID: &str = "uid";

/// A verified agent, read out of a tenant-pinned access token.
#[derive(Debug, Clone)]
pub struct McpAgent {
    /// The agent row's id, which is the token's `sub`.
    pub agent_id: i64,
    /// The tenant slug this token is pinned to.
    pub tenant: String,
    /// The granted skill codenames.
    pub skills: Vec<String>,
    /// The tools those skills add up to.
    pub tools: Vec<String>,
    /// The user who owns the key, or `None` for a machine agent. A
    /// tool handler scopes its work to this user.
    pub user_id: Option<i64>,
    /// The token id, which is what revocation acts on.
    pub jti: String,
}

/// Issue a short-lived access token for `agent_id`, pinned to
/// `tenant` and carrying its grants.
///
/// # Errors
/// [`JwtIssueError`] if a custom claim collides with a reserved
/// name, which none of these do.
pub fn issue_agent_token(
    jwt: &JwtLifecycle,
    agent_id: i64,
    tenant: &str,
    skills: &[String],
    tools: &[String],
    user_id: Option<i64>,
) -> Result<String, JwtIssueError> {
    let mut custom = serde_json::Map::new();
    custom.insert(CLAIM_KIND.into(), json!(KIND_AGENT));
    custom.insert(CLAIM_TENANT.into(), json!(tenant));
    custom.insert(CLAIM_SKILLS.into(), json!(skills));
    custom.insert(CLAIM_TOOLS.into(), json!(tools));
    if let Some(uid) = user_id {
        custom.insert(CLAIM_UID.into(), json!(uid));
    }
    jwt.issue_access_with(agent_id, custom)
}

/// Verify an agent token against `expected_tenant` and return the
/// [`McpAgent`]. Anything wrong gives `None`: a bad signature, an
/// expired or revoked token, a token that is not an agent token, or
/// the wrong tenant.
///
/// It is async because the revocation check may read a durable
/// [`crate::jti_store::JtiStore`].
#[must_use]
pub async fn verify_agent_token(
    jwt: &JwtLifecycle,
    token: &str,
    expected_tenant: &str,
) -> Option<McpAgent> {
    let claims = jwt.verify_access(token).await?;
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
        user_id: claims.get_custom::<i64>(CLAIM_UID),
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

/// What [`mint_agent_jwt`] produces. Both token endpoints render it
/// in their own shape.
pub(crate) struct MintedToken {
    pub token: String,
    pub expires_in: i64,
    /// The granted skills, space-separated, as OAuth's `scope`.
    pub scope: String,
}

pub(crate) enum MintError {
    /// The name or secret was wrong.
    Unauthorized,
    /// Something failed on our side.
    Internal,
}

/// Check `{name, secret}` against the tenant, resolve the agent's
/// grants, and issue a tenant-pinned token. Both `/token` and
/// `/oauth/token` go through here.
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
    // A user-owned key takes its capabilities from the owner's
    // permissions. A machine agent has only its own grants.
    let grants = match agent.user_id {
        Some(uid) => crate::tenancy::resolve_user_agent_grants_pool(pool, agent_id, uid).await,
        None => crate::tenancy::resolve_agent_grants_pool(pool, agent_id).await,
    };
    let (skills, tools) = grants.map_err(|e| {
        tracing::warn!(error = %e, "mcp grant resolution failed");
        MintError::Internal
    })?;
    let token =
        issue_agent_token(jwt, agent_id, slug, &skills, &tools, agent.user_id).map_err(|e| {
            tracing::error!(error = %e, "mcp token issuance failed");
            MintError::Internal
        })?;
    Ok(MintedToken {
        token,
        expires_in: jwt.access_ttl_secs,
        scope: skills.join(" "),
    })
}

/// `POST {prefix}/token`: trade `{name, secret}` for a scoped token,
/// pinned to the request's tenant.
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

/// Why a bearer token was refused. It is separate from the response
/// so the JSON-RPC POST and the SSE GET render it the same way.
pub(crate) enum BearerRejection {
    /// The token matched no accepted shape, or the agent is revoked
    /// or deactivated.
    Unauthorized,
    /// The liveness lookup itself failed. That is our problem, not
    /// the caller's, so it must never be a 401: a client reads 401
    /// as "start an OAuth flow" and would chase a sign-in that was
    /// never the issue.
    CheckFailed,
}

impl BearerRejection {
    pub(crate) fn into_response(self, headers: &HeaderMap, uri: &axum::http::Uri) -> Response {
        match self {
            Self::Unauthorized => unauthorized(headers, uri),
            Self::CheckFailed => {
                (StatusCode::INTERNAL_SERVER_ERROR, "auth check failed").into_response()
            }
        }
    }
}

/// Resolve a bearer token to a live agent, pinned to one tenant.
///
/// Two token shapes are accepted: a minted JWT, or the raw
/// `prefix.secret` credential. The raw one is the copy-paste key a
/// member generates in the app's UI, and it works in any MCP client
/// with no exchange step. [`verify_raw_agent_credential`] checks
/// liveness and resolves grants on every request, so a permission
/// change or a revocation takes effect at once, with no JWT window
/// to wait out.
///
/// **Every** authenticated MCP surface must call this, not only the
/// JSON-RPC POST. If the SSE GET verified JWTs alone, a raw key
/// would work on POST and 401 on GET. That is not a cosmetic
/// difference: a client reads the 401 as "this resource wants
/// OAuth", drops the bearer that was already working, and walks the
/// discovery and registration path instead. That fails elsewhere and
/// reports *that* as the error, so the connection is dead and the
/// log points at the wrong thing.
pub(crate) async fn authenticate_bearer(
    jwt: &JwtLifecycle,
    pool: &crate::sql::Pool,
    slug: &str,
    token: &str,
) -> Result<McpAgent, BearerRejection> {
    match verify_agent_token(jwt, token, slug).await {
        Some(agent) => {
            // The JWT holds no state, so check here that the agent,
            // and the owner of a user-owned key, still exist and are
            // active. A revoked key is then refused at once instead
            // of working until it expires.
            match crate::tenancy::agent_token_still_valid_pool(pool, agent.agent_id, agent.user_id)
                .await
            {
                Ok(true) => Ok(agent),
                Ok(false) => Err(BearerRejection::Unauthorized),
                Err(e) => {
                    tracing::warn!(error = %e, "mcp agent liveness re-check failed");
                    Err(BearerRejection::CheckFailed)
                }
            }
        }
        None => verify_raw_agent_credential(pool, slug, token)
            .await
            .ok_or(BearerRejection::Unauthorized),
    }
}

/// `POST {prefix}`, authenticated: require an agent token, then run
/// the JSON-RPC message. A token for another tenant, or a revoked or
/// expired one, is refused.
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
    let agent = match authenticate_bearer(jwt, t.pool(), &t.org.slug, token).await {
        Ok(agent) => agent,
        Err(e) => return e.into_response(&headers, &uri),
    };
    // The agent is verified, so hand the tools layer this tenant's
    // pool along with it, and `tools/call` runs on the right tenant.
    let ctx = super::tools::McpContext {
        pool: t.pool().clone(),
        agent,
        // `call_tool_with` sets these up per call.
        progress: super::progress::ProgressReporter::disabled(),
        cancel: super::progress::CancelToken::never(),
    };
    handle_message(&state, &body, Some(ctx)).await
}

/// Resolve a raw `prefix.secret` credential used straight as the
/// bearer token. It returns the same [`McpAgent`] that
/// [`verify_agent_token`] does, or `None` on any failure.
///
/// This is the copy-paste path: the show-once key a member generates
/// works in any MCP client as `Authorization: Bearer
/// <prefix.secret>`, with no exchange step. Liveness and grants are
/// resolved on **every request**, so a user-owned key always
/// reflects its owner's current permissions, and revocation is
/// immediate. That is fresher than the snapshot a JWT carries.
///
/// The Argon2 check is the expensive part, so a successful one is
/// remembered for [`RAW_KEY_CACHE_TTL`] in a small bounded cache,
/// keyed by tenant and token hash. Only the hash check is skipped on
/// a hit; liveness and grants are never cached.
///
/// A junk token never reaches Argon2: it must split into
/// `prefix.secret` first. And
/// [`crate::tenancy::authenticate_agent_by_prefix_pool`] runs a
/// dummy verification for an unknown prefix, so the timing gives
/// nothing away.
pub async fn verify_raw_agent_credential(
    pool: &crate::sql::Pool,
    slug: &str,
    token: &str,
) -> Option<McpAgent> {
    // A credential is `<8-hex prefix>.<hex secret>`; see
    // `tenancy::agents::generate_credential`. Anything else, a JWT
    // or a random string, is refused before any database or Argon2
    // work happens.
    let (prefix, secret) = token.split_once('.')?;
    let is_hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if prefix.len() != 8 || !is_hex(prefix) || !is_hex(secret) {
        return None;
    }

    let cache_key = raw_key_cache_key(slug, token);
    let agent_id = match raw_key_cache_get(&cache_key) {
        Some(id) => id,
        None => {
            let agent =
                match crate::tenancy::authenticate_agent_by_prefix_pool(pool, prefix, secret).await
                {
                    Ok(Some(a)) => a,
                    Ok(None) => return None,
                    Err(e) => {
                        tracing::warn!(error = %e, "mcp raw-key authentication failed");
                        return None;
                    }
                };
            let agent_id = agent.id.get().copied().unwrap_or_default();
            raw_key_cache_put(cache_key, agent_id);
            agent_id
        }
    };

    // The row decides, on every request, cache hit or not.
    //
    // The cache holds one fact: this token hashed to this agent at
    // some earlier time. Nothing authorization depends on is in
    // there. `user_id` picks the grant resolver, so it is read from
    // the row; caching it would keep using the wrong resolver for
    // the rest of the TTL after an owner change.
    let state = match crate::tenancy::agent_auth_state_pool(pool, agent_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "mcp raw-key liveness check failed");
            return None;
        }
    };
    if !state.active {
        raw_key_cache_forget(&cache_key);
        return None;
    }
    // Check the cached id against the credential actually presented.
    //
    // A cache entry only says that some token hashed to agent N in
    // the past. `secret_prefix` is random per credential and is
    // regenerated on every rotation, so comparing it with the prefix
    // in hand answers the two questions the entry cannot: has the
    // secret rotated since, and does this row even belong to this
    // credential?
    //
    // Two other approaches do not work here. Clearing the cache on
    // rotation fails because rotation runs in the `manage` CLI, a
    // different process from the server, so the serving replica
    // never hears about it. Comparing timestamps fails even when the
    // clocks agree: a rotation stamps before its own commit
    // while a verify stamps after an Argon2 of about 11 ms, and both
    // biases make a rotation inside that window look older than the
    // verify. A string comparison needs no clock at all.
    if state.secret_prefix != prefix {
        raw_key_cache_forget(&cache_key);
        return None;
    }
    let user_id = state.user_id;
    if let Some(uid) = user_id {
        let owner_active = match crate::tenancy::agent_owner_is_active_pool(pool, uid).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "mcp raw-key owner check failed");
                return None;
            }
        };
        if !owner_active {
            return None;
        }
    }

    // Grants resolve on every request, and are never cached.
    let grants = match user_id {
        Some(uid) => crate::tenancy::resolve_user_agent_grants_pool(pool, agent_id, uid).await,
        None => crate::tenancy::resolve_agent_grants_pool(pool, agent_id).await,
    };
    let (skills, tools) = match grants {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "mcp raw-key grant resolution failed");
            return None;
        }
    };
    Some(McpAgent {
        agent_id,
        tenant: slug.to_owned(),
        skills,
        tools,
        user_id,
        jti: format!("raw:{agent_id}"),
    })
}

/// How long a successful raw-key verification lets us skip Argon2.
const RAW_KEY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// How many verifications the cache may hold before it evicts.
const RAW_KEY_CACHE_CAP: usize = 256;

/// Maps a token hash to an agent id and the time it was verified.
///
/// The id is all it holds. Everything authorization depends on — the
/// owner, the grants, whether the row still accepts this credential
/// — is read from the row on each request. So a hit cannot carry a
/// stale owner or a stale permission set, and even the id is checked
/// against the presented credential before use.
///
/// The time is an `Instant`, not a wall clock, so the TTL does not
/// move when the system clock does.
type RawKeyCache = std::collections::HashMap<[u8; 32], (i64, std::time::Instant)>;

fn raw_key_cache() -> &'static std::sync::Mutex<RawKeyCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<RawKeyCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The cache key: SHA-256 over the tenant and the token together.
/// The tenant is in there so a credential verified against one
/// tenant's pool can never authenticate on another.
fn raw_key_cache_key(slug: &str, token: &str) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(slug.as_bytes());
    hasher.update([0u8]);
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

fn raw_key_cache_get(key: &[u8; 32]) -> Option<i64> {
    let cache = raw_key_cache().lock().ok()?;
    let (agent_id, cached_at) = cache.get(key)?;
    (cached_at.elapsed() < RAW_KEY_CACHE_TTL).then_some(*agent_id)
}

/// Drop one entry, for when the row shows it is stale. The next
/// request then re-runs Argon2, instead of repeating a lookup that
/// will be rejected again for the rest of the TTL.
fn raw_key_cache_forget(key: &[u8; 32]) {
    if let Ok(mut cache) = raw_key_cache().lock() {
        cache.remove(key);
    }
}

fn raw_key_cache_put(key: [u8; 32], agent_id: i64) {
    let Ok(mut cache) = raw_key_cache().lock() else {
        return;
    };
    if cache.len() >= RAW_KEY_CACHE_CAP {
        // Drop the expired entries first. If that is not enough,
        // clear the lot: this is only an optimization, and refilling
        // costs one Argon2 per key.
        cache.retain(|_, (_, at)| at.elapsed() < RAW_KEY_CACHE_TTL);
        if cache.len() >= RAW_KEY_CACHE_CAP {
            cache.clear();
        }
    }
    cache.insert(key, (agent_id, std::time::Instant::now()));
}

// There is deliberately no `invalidate_raw_key_cache(agent_id)`.
// This cache is per-process, and every caller that would want it
// runs in the `manage` CLI, so it could never reach a serving
// replica. Rotation is spotted from the row instead, which works
// across processes.

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// The request's origin, `scheme://host`, read from the headers. It
/// honours `X-Forwarded-Proto`, and otherwise assumes `http` on
/// localhost and `https` anywhere else.
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

/// The public base URL the MCP routes are mounted at. It is the
/// request origin plus the original path, before nesting, with
/// `strip_suffix` and any trailing slash removed. That way the
/// discovery documents follow the real `.nest(prefix)` mount instead
/// of assuming the origin root.
///
/// With origin `https://h` and original path
/// `/mcp/.well-known/oauth-protected-resource`, stripping
/// `/.well-known/oauth-protected-resource` gives `https://h/mcp`.
/// For the JSON-RPC endpoint the path already is the prefix, so pass
/// an empty `strip_suffix`.
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

/// A `401` whose `WWW-Authenticate` header carries a
/// `resource_metadata` URL, so a standards-compliant client can find
/// the authorization server. The URL follows the real mount prefix,
/// not the origin root.
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

/// The default [`JwtLifecycle`] for the MCP auth router. Its key
/// comes from `RUSTANGO_SESSION_SECRET`, as the rest of the
/// framework's does.
pub(crate) fn default_jwt() -> Arc<JwtLifecycle> {
    Arc::new(JwtLifecycle::new(jwt_secret()))
}

/// The key that signs agent tokens: `RUSTANGO_SESSION_SECRET`, which
/// the rest of the framework uses too.
///
/// With that unset it warns and uses a random key for this process,
/// so dev works but nothing survives a restart. `manage check
/// --deploy` reports a missing or short secret.
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

    #[tokio::test]
    async fn token_round_trips_the_owning_user_id() {
        use crate::tenancy::jwt_lifecycle::JwtLifecycle;
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());

        // A user-owned key carries `uid`.
        let token = issue_agent_token(
            &jwt,
            5,
            "acme",
            &["coach".into()],
            &["log".into()],
            Some(99),
        )
        .expect("issue");
        let agent = verify_agent_token(&jwt, &token, "acme")
            .await
            .expect("verify");
        assert_eq!(agent.agent_id, 5);
        assert_eq!(agent.user_id, Some(99));
        assert_eq!(agent.tools, vec!["log"]);

        // A machine agent has no `uid`.
        let token = issue_agent_token(&jwt, 5, "acme", &[], &[], None).expect("issue");
        assert_eq!(
            verify_agent_token(&jwt, &token, "acme")
                .await
                .expect("verify")
                .user_id,
            None
        );
    }

    fn headers(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, host.parse().unwrap());
        h
    }

    #[test]
    fn mount_base_tracks_the_nest_prefix() {
        let h = headers("app.example");
        // On the JSON-RPC endpoint the path already is the prefix.
        let uri: Uri = "/mcp".parse().unwrap();
        assert_eq!(mount_base(&h, &uri, ""), "https://app.example/mcp");
        // On a discovery document, strip the suffix off to get it.
        let uri: Uri = "/api/mcp/.well-known/oauth-protected-resource"
            .parse()
            .unwrap();
        assert_eq!(
            mount_base(&h, &uri, "/.well-known/oauth-protected-resource"),
            "https://app.example/api/mcp"
        );
        // A mount at the origin root gives an empty prefix, with no
        // trailing slash left behind.
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

    /// The raw-key cache is shared, so these tests take turns.
    fn cache_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    // Rotation and cross-tenant redemption are checked in
    // `tests/mcp_raw_key.rs`, which has a `Pool` and can call
    // `verify_raw_agent_credential`. They cannot be checked here:
    // an earlier attempt only asserted `chrono`'s `>` operator, and
    // stayed green even with the whole rotation check deleted. What
    // is left in this module is what it can decide on its own.

    #[test]
    fn a_cached_entry_round_trips_and_can_be_forgotten() {
        let _g = cache_lock();
        raw_key_cache().lock().unwrap().clear();

        let key = raw_key_cache_key("acme", "pfx.secret");
        assert_eq!(raw_key_cache_get(&key), None, "cold cache");

        raw_key_cache_put(key, 7);
        assert_eq!(raw_key_cache_get(&key), Some(7));

        raw_key_cache_forget(&key);
        assert_eq!(
            raw_key_cache_get(&key),
            None,
            "a forgotten entry must not be served again — the reject \
             paths call this so the next request re-runs Argon2 rather \
             than repeating the same refusal for the rest of the TTL",
        );
    }

    #[test]
    fn the_cache_key_is_tenant_scoped() {
        // Two tenants both having an agent 7 is normal, because ids
        // come from per-tenant sequences. The cache key must keep
        // them apart.
        let _g = cache_lock();
        assert_ne!(
            raw_key_cache_key("acme", "pfx.secret"),
            raw_key_cache_key("globex", "pfx.secret"),
            "the same token under two tenants must not share a slot",
        );
    }
}
