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

use crate::extractors::{Tenant, TenantScope};
use crate::login_throttle::{ClientIp, LoginRefused, LoginScope};
use crate::tenancy::jwt_lifecycle::{JwtIssueError, JwtLifecycle};
use crate::tenancy::{AgentError, TenancyError, TenantSlug};

use super::router::AuthedMcpState;
use super::transport::handle_message;

/// Claim naming what kind of principal this is; see [`KIND_AGENT`].
pub const CLAIM_KIND: &str = crate::tenancy::jwt_lifecycle::CLAIM_KIND;
/// The [`CLAIM_KIND`] value an agent token carries.
pub const KIND_AGENT: &str = "agent";
/// Claim pinning the token to one tenant slug.
pub const CLAIM_TENANT: &str = crate::tenancy::jwt_lifecycle::CLAIM_TENANT;
/// Claim pinning the token to that tenant's org id.
pub const CLAIM_ORG_ID: &str = crate::tenancy::jwt_lifecycle::CLAIM_ORG_ID;
/// Claim listing the agent's granted skill codenames.
pub const CLAIM_SKILLS: &str = "skills";
/// Claim listing the tools those skills add up to.
pub const CLAIM_TOOLS: &str = "tools";
/// Claim naming the user who owns the key. A machine agent has none.
pub const CLAIM_UID: &str = "uid";
/// Claim naming the credential the token was minted from; a rotation ends it.
pub const CLAIM_KEY_PREFIX: &str = "key_prefix";

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
    /// The public prefix of the credential this agent signed in with.
    pub secret_prefix: String,
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
    tenant: &TenantSlug,
    skills: &[String],
    tools: &[String],
    user_id: Option<i64>,
    secret_prefix: &str,
) -> Result<String, JwtIssueError> {
    let mut custom = serde_json::Map::new();
    custom.insert(CLAIM_KIND.into(), json!(KIND_AGENT));
    custom.insert(CLAIM_KEY_PREFIX.into(), json!(secret_prefix));
    custom.insert(CLAIM_TENANT.into(), json!(tenant.slug));
    custom.insert(CLAIM_ORG_ID.into(), json!(tenant.org_id));
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
///
/// Skills and tools are as at mint time, and the secret is not checked for
/// rotation; the live path re-checks both via [`crate::tenancy::agent_token_still_valid_pool`].
#[must_use]
pub async fn verify_agent_token(
    jwt: &JwtLifecycle,
    token: &str,
    expected_tenant: &TenantSlug,
) -> Option<McpAgent> {
    verify_agent_claims(jwt, token, expected_tenant)
        .await
        .map(|(agent, _)| agent)
}

/// [`verify_agent_token`] plus the token's `exp`, in unix seconds.
async fn verify_agent_claims(
    jwt: &JwtLifecycle,
    token: &str,
    expected_tenant: &TenantSlug,
) -> Option<(McpAgent, i64)> {
    let claims = jwt.verify_access(token).await?;
    if claims.get_custom::<String>(CLAIM_KIND).as_deref() != Some(KIND_AGENT) {
        return None;
    }
    let tenant = claims.get_custom::<String>(CLAIM_TENANT)?;
    // The org id too: a tenant that reuses a purged slug refuses its tokens.
    let org_id = claims.get_custom::<i64>(CLAIM_ORG_ID)?;
    if !expected_tenant.binds(&tenant, org_id) {
        return None;
    }
    // A token from before the claim existed cannot be checked for rotation.
    let secret_prefix = claims.get_custom::<String>(CLAIM_KEY_PREFIX)?;
    let agent = McpAgent {
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
        secret_prefix,
    };
    Some((agent, claims.exp))
}

// --------------------------------------------------------------- HTTP layer

// No `Debug` on these: they carry a secret or a bearer token.
#[derive(Deserialize)]
pub(crate) struct AgentTokenInput {
    pub name: String,
    pub secret: String,
}

#[derive(Serialize)]
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
    /// Too many failed attempts (`429`), or no hashing slot freed up (`503`).
    Refused(LoginRefused),
}

/// One agent-secret check through the login gate. Failures count per IP
/// and per tenant, as API keys do; there is no account lock.
async fn gated<T>(
    slug: &str,
    ip: &ClientIp,
    check: impl std::future::Future<Output = Result<Option<T>, AgentError>>,
) -> Result<Result<Option<T>, AgentError>, LoginRefused> {
    let scope = LoginScope::TenantAgent(slug.to_owned());
    let attempt = crate::login_throttle::shared()
        .begin(&scope, ip, "")
        .await?;
    let checked = check.await;
    match &checked {
        Ok(Some(_)) => attempt.succeeded().await,
        Ok(None) => attempt.failed().await,
        // Busy or a DB error checked nothing: dropping gives the tokens back.
        Err(_) => drop(attempt),
    }
    Ok(checked)
}

/// Check `{name, secret}` against the tenant, resolve the agent's
/// grants, and issue a tenant-pinned token. Both `/token` and
/// `/oauth/token` go through here.
pub(crate) async fn mint_agent_jwt(
    jwt: &JwtLifecycle,
    pool: &crate::sql::Pool,
    tenant: &TenantSlug,
    name: &str,
    secret: &str,
    ip: &ClientIp,
) -> Result<MintedToken, MintError> {
    let check = crate::tenancy::authenticate_agent_pool(pool, name, secret);
    let agent = match gated(&tenant.slug, ip, check)
        .await
        .map_err(MintError::Refused)?
    {
        Ok(Some(a)) => a,
        Ok(None) => return Err(MintError::Unauthorized),
        Err(AgentError::Tenancy(TenancyError::Busy)) => {
            return Err(MintError::Refused(LoginRefused::Busy))
        }
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
    let token = issue_agent_token(
        jwt,
        agent_id,
        tenant,
        &skills,
        &tools,
        agent.user_id,
        &agent.secret_prefix,
    )
    .map_err(|e| {
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
pub(crate) fn agent_token<DB: crate::sql::sqlx::Database>(
    t: Tenant<DB>,
    State(state): State<AuthedMcpState>,
    ip: ClientIp,
    Json(input): Json<AgentTokenInput>,
) -> impl std::future::Future<Output = Response> + Send {
    agent_token_in(t.into(), state, ip, input)
}

async fn agent_token_in(
    t: TenantScope,
    state: AuthedMcpState,
    ip: ClientIp,
    input: AgentTokenInput,
) -> Response {
    let Some(tenant) = TenantSlug::of(&t.org) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tenant has no id").into_response();
    };
    match mint_agent_jwt(
        &state.jwt,
        t.pool(),
        &tenant,
        &input.name,
        &input.secret,
        &ip,
    )
    .await
    {
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
        Err(MintError::Refused(r)) => r.into_response(),
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
    /// Too many failed raw keys (`429`), or no hashing slot freed up
    /// (`503`); never a 401.
    Refused(LoginRefused),
}

/// A raw-key check that failed: busy is 503, anything else 500.
impl From<AgentError> for BearerRejection {
    fn from(e: AgentError) -> Self {
        if matches!(e, AgentError::Tenancy(TenancyError::Busy)) {
            return Self::Refused(LoginRefused::Busy);
        }
        tracing::warn!(error = %e, "mcp raw-key check failed");
        Self::CheckFailed
    }
}

impl BearerRejection {
    pub(crate) fn into_response(
        self,
        headers: &HeaderMap,
        extensions: &axum::http::Extensions,
        uri: &axum::http::Uri,
    ) -> Response {
        match self {
            Self::Unauthorized => unauthorized(headers, extensions, uri),
            Self::CheckFailed => {
                (StatusCode::INTERNAL_SERVER_ERROR, "auth check failed").into_response()
            }
            Self::Refused(r) => r.into_response(),
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
    tenant: &TenantSlug,
    token: &str,
    ip: &ClientIp,
) -> Result<McpAgent, BearerRejection> {
    authenticate_bearer_until(jwt, pool, tenant, token, ip)
        .await
        .map(|(agent, _)| agent)
}

/// [`authenticate_bearer`] plus the JWT's `exp` (unix seconds); a raw key has none.
pub(crate) async fn authenticate_bearer_until(
    jwt: &JwtLifecycle,
    pool: &crate::sql::Pool,
    tenant: &TenantSlug,
    token: &str,
    ip: &ClientIp,
) -> Result<(McpAgent, Option<i64>), BearerRejection> {
    match verify_agent_claims(jwt, token, tenant).await {
        Some((mut agent, exp)) => {
            // The JWT holds no state, so check here that the agent,
            // and the owner of a user-owned key, still exist and are
            // active, and that its secret was not rotated. A revoked
            // key is then refused at once instead of working until it
            // expires.
            let live = match crate::tenancy::agents::LiveAgent::load(pool, agent.agent_id).await {
                Ok(Some(live))
                    if live.agent.user_id == agent.user_id
                        && live.agent.secret_prefix == agent.secret_prefix =>
                {
                    live
                }
                Ok(_) => return Err(BearerRejection::Unauthorized),
                Err(e) => {
                    tracing::warn!(error = %e, "mcp agent liveness re-check failed");
                    return Err(BearerRejection::CheckFailed);
                }
            };
            // Grants come from the rows, not the claims, as on the raw-key path (#1962).
            (agent.skills, agent.tools) = live.grants(pool).await?;
            Ok((agent, Some(exp)))
        }
        None => {
            let Some((prefix, secret)) = split_raw_credential(token) else {
                return Err(BearerRejection::Unauthorized);
            };
            // Only a cache miss runs this, so only Argon2 checks count.
            let hash_check = async {
                let check = crate::tenancy::authenticate_agent_by_prefix_pool(pool, prefix, secret);
                match gated(&tenant.slug, ip, check).await {
                    Ok(checked) => checked.map_err(BearerRejection::from),
                    Err(refused) => Err(BearerRejection::Refused(refused)),
                }
            };
            match verify_raw_with(pool, tenant, token, prefix, hash_check).await? {
                Some(agent) => Ok((agent, None)),
                None => Err(BearerRejection::Unauthorized),
            }
        }
    }
}

/// `POST {prefix}`, authenticated: require an agent token, then run
/// the JSON-RPC message. A token for another tenant, or a revoked or
/// expired one, is refused.
pub(crate) fn post_authed<DB: crate::sql::sqlx::Database>(
    t: Tenant<DB>,
    State(state): State<AuthedMcpState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    body: Bytes,
) -> impl std::future::Future<Output = Response> + Send {
    post_authed_in(t.into(), state, uri, headers, extensions, body)
}

async fn post_authed_in(
    t: TenantScope,
    state: AuthedMcpState,
    uri: axum::http::Uri,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    body: Bytes,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return unauthorized(&headers, &extensions, &uri);
    };
    let ip = ClientIp::from_parts(&extensions, &headers);
    let Some(tenant) = TenantSlug::of(&t.org) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tenant has no id").into_response();
    };
    let agent = match authenticate_bearer(&state.jwt, t.pool(), &tenant, token, &ip).await {
        Ok(agent) => agent,
        Err(e) => return e.into_response(&headers, &extensions, &uri),
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
    handle_message(&state.mcp, &body, Some(ctx)).await
}

/// Resolve a raw `prefix.secret` credential used straight as the
/// bearer token. It returns the same [`McpAgent`] that
/// [`verify_agent_token`] does, `Ok(None)` for a refused credential,
/// or an error when the check itself failed (a busy hash queue is
/// `TenancyError::Busy`, answer 503).
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
///
/// It does not count failures: the MCP routes do, per client IP, through
/// [`crate::login_throttle`].
///
/// # Errors
/// [`crate::tenancy::AgentError`] when a lookup or the hash check fails.
pub async fn verify_raw_agent_credential(
    pool: &crate::sql::Pool,
    tenant: &TenantSlug,
    token: &str,
) -> Result<Option<McpAgent>, AgentError> {
    let Some((prefix, secret)) = split_raw_credential(token) else {
        return Ok(None);
    };
    let hash_check = crate::tenancy::authenticate_agent_by_prefix_pool(pool, prefix, secret);
    verify_raw_with(pool, tenant, token, prefix, hash_check).await
}

/// Split a credential `<8-hex prefix>.<hex secret>`; see
/// `tenancy::agents::generate_credential`. Anything else, a JWT or a
/// random string, is refused before any database or Argon2 work.
fn split_raw_credential(token: &str) -> Option<(&str, &str)> {
    let (prefix, secret) = token.split_once('.')?;
    let is_hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    (prefix.len() == 8 && is_hex(prefix) && is_hex(secret)).then_some((prefix, secret))
}

/// [`verify_raw_agent_credential`] with the Argon2 check passed in; it
/// is awaited only on a cache miss.
async fn verify_raw_with<E: From<AgentError>>(
    pool: &crate::sql::Pool,
    tenant: &TenantSlug,
    token: &str,
    prefix: &str,
    hash_check: impl std::future::Future<Output = Result<Option<crate::tenancy::Agent>, E>>,
) -> Result<Option<McpAgent>, E> {
    let cache_key = raw_key_cache_key(&tenant.namespace(), token);
    let agent_id = match raw_key_cache_get(&cache_key) {
        Some(id) => id,
        None => {
            let Some(agent) = hash_check.await? else {
                return Ok(None);
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
    // `LiveAgent::load` refuses an inactive key or owner.
    let Some(live) = crate::tenancy::agents::LiveAgent::load(pool, agent_id).await? else {
        raw_key_cache_forget(&cache_key);
        return Ok(None);
    };
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
    if live.agent.secret_prefix != prefix {
        raw_key_cache_forget(&cache_key);
        return Ok(None);
    }

    // Grants resolve on every request, and are never cached.
    let (skills, tools) = live.grants(pool).await?;
    Ok(Some(McpAgent {
        agent_id,
        tenant: tenant.slug.clone(),
        skills,
        tools,
        user_id: live.agent.user_id,
        jti: format!("raw:{agent_id}"),
        secret_prefix: live.agent.secret_prefix,
    }))
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
fn raw_key_cache_key(namespace: &str, token: &str) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(namespace.as_bytes());
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
    if cache.len() >= RAW_KEY_CACHE_CAP && !cache.contains_key(&key) {
        // Drop the expired entries first, then the oldest one; a full
        // clear would send every active key back to Argon2 (#2301).
        cache.retain(|_, (_, at)| at.elapsed() < RAW_KEY_CACHE_TTL);
        if cache.len() >= RAW_KEY_CACHE_CAP {
            let oldest = cache.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            }
        }
    }
    cache.insert(key, (agent_id, std::time::Instant::now()));
}

/// One lock for every crate test that reads or writes the shared raw-key cache.
#[cfg(test)]
pub(crate) fn raw_key_cache_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &LOCK
}

// There is deliberately no `invalidate_raw_key_cache(agent_id)`.
// This cache is per-process, and every caller that would want it
// runs in the `manage` CLI, so it could never reach a serving
// replica. Rotation is spotted from the row instead, which works
// across processes.

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    crate::bearer::token(headers)
}

/// The request's origin, `scheme://host`, read from the headers. It
/// honours `X-Forwarded-Proto` from a trusted proxy, and otherwise
/// assumes `http` on a loopback host and `https` anywhere else.
pub(crate) fn origin(headers: &HeaderMap, extensions: &axum::http::Extensions) -> String {
    // Only a plain `host[:port]` goes into the discovery URLs (#1963).
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(crate::urls::HostAuthority::parse)
        .map_or_else(|| "localhost".to_owned(), |h| h.to_string());
    let scheme = crate::real_ip::trusted_forwarded(headers, extensions, "x-forwarded-proto")
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if crate::urls::is_loopback_host(&host) {
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
    extensions: &axum::http::Extensions,
    original: &axum::http::Uri,
    strip_suffix: &str,
) -> String {
    let path = original.path();
    let prefix = path
        .strip_suffix(strip_suffix)
        .unwrap_or(path)
        .trim_end_matches('/');
    format!("{}{}", origin(headers, extensions), prefix)
}

/// A `401` whose `WWW-Authenticate` header carries a
/// `resource_metadata` URL, so a standards-compliant client can find
/// the authorization server. The URL follows the real mount prefix,
/// not the origin root. The body is a JSON-RPC error, which MCP
/// clients parse and show.
pub(crate) fn unauthorized(
    headers: &HeaderMap,
    extensions: &axum::http::Extensions,
    original: &axum::http::Uri,
) -> Response {
    let base = mount_base(headers, extensions, original, "");
    let challenge =
        format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#);
    let body = super::types::JsonRpcResponse::failure(
        serde_json::Value::Null,
        super::types::JsonRpcError::new(
            super::types::codes::UNAUTHORIZED,
            "missing or invalid agent token",
        ),
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        Json(body),
    )
        .into_response()
}

/// The default [`JwtLifecycle`] for the MCP auth router. Its key
/// comes from `RUSTANGO_SESSION_SECRET`, as the rest of the
/// framework's does.
pub(crate) fn default_jwt() -> Arc<JwtLifecycle> {
    Arc::new(JwtLifecycle::lazy(jwt_secret))
}

/// The key that signs agent tokens: `RUSTANGO_SESSION_SECRET`, which
/// the rest of the framework uses too.
///
/// With that unset, off the prod tier it is the tenant session key saved
/// in `./var/`, so dev tokens survive a restart. On prod it warns and uses
/// a random key for this process; `manage check --deploy` reports that.
pub(crate) fn jwt_secret() -> Vec<u8> {
    agent_secret(
        std::env::var("RUSTANGO_SESSION_SECRET").ok().as_deref(),
        &crate::session::tier_from_env(),
        std::path::Path::new(crate::session::TENANT_SESSION_KEY_PATH),
    )
}

/// A secret that is not a valid key (a passphrase) never signs: its text
/// is guessable. It falls back as an unset one does.
fn agent_secret(raw: Option<&str>, tier: &str, disk: &std::path::Path) -> Vec<u8> {
    let unusable = match agent_key(raw) {
        AgentKey::Valid(key) => return key,
        AgentKey::Unusable(e) => Some(e),
        AgentKey::Missing => None,
    };
    if !crate::session::is_prod_tier(tier) {
        // Warns itself when `raw` is set but unusable.
        return crate::session::SessionSecret::from_raw_or_disk(raw, disk)
            .key()
            .to_vec();
    }
    use rand::RngCore;
    match unusable {
        Some(e) => tracing::warn!(
            reason = %e,
            "RUSTANGO_SESSION_SECRET is not a valid base64 key, so MCP agent tokens do not \
             sign with it; they use an ephemeral per-process key. Generate one with: \
             openssl rand -base64 32"
        ),
        None => tracing::warn!(
            "RUSTANGO_SESSION_SECRET unset or <32 bytes; MCP agent tokens \
             are signed with an ephemeral per-process key (tokens won't \
             survive a restart and won't verify across instances)"
        ),
    }
    let mut key = vec![0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

/// What `RUSTANGO_SESSION_SECRET` gives the agent-token key.
enum AgentKey {
    /// A secret [`crate::session::SessionSecret::from_b64`] accepts. The
    /// key stays its text, which issued tokens were signed with.
    Valid(Vec<u8>),
    /// 32 bytes or more, but not a valid secret: refused.
    Unusable(crate::session::SessionSecretError),
    /// Unset, or shorter than 32 bytes.
    Missing,
}

fn agent_key(raw: Option<&str>) -> AgentKey {
    let Some(raw) = raw.filter(|r| r.len() >= 32) else {
        return AgentKey::Missing;
    };
    match crate::session::SessionSecret::from_b64(raw) {
        Ok(_) => AgentKey::Valid(raw.as_bytes().to_vec()),
        Err(e) => AgentKey::Unusable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    /// Off prod an unset secret signs with the saved session key, so dev
    /// tokens survive a restart; prod gets a per-process key and no file.
    #[test]
    fn dev_agent_tokens_sign_with_the_saved_session_key() {
        let dir = std::env::temp_dir().join(format!("rustango_mcp_key_{}", std::process::id()));
        let path = dir.join("var/.rustango_tenant_session.key");
        let first = agent_secret(None, "dev", &path);
        assert_eq!(std::fs::read(&path).unwrap(), first, "not the saved key");
        assert_eq!(
            agent_secret(None, "dev", &path),
            first,
            "changed on restart"
        );
        let prod = dir.join("prod.key");
        assert_eq!(agent_secret(None, "prod", &prod).len(), 32);
        assert!(!prod.exists(), "prod wrote a key file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A passphrase never becomes the signing key, on any tier: off prod it
    /// is the saved session key, on prod a per-process one.
    #[test]
    fn a_passphrase_secret_never_signs_agent_tokens() {
        let pass = "correct horse battery staple and two more";
        let dir = std::env::temp_dir().join(format!("rustango_mcp_pass_{}", std::process::id()));
        let path = dir.join("var/.rustango_tenant_session.key");
        let dev = agent_secret(Some(pass), "dev", &path);
        assert_ne!(dev, pass.as_bytes(), "dev signs with the passphrase");
        assert_eq!(std::fs::read(&path).unwrap(), dev, "not the saved key");
        let prod = agent_secret(Some(pass), "prod", &dir.join("prod.key"));
        assert_ne!(prod, pass.as_bytes(), "prod signs with the passphrase");
        assert_eq!(prod.len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Building the router (every CLI verb does) reads no key and logs nothing.
    #[test]
    fn building_the_router_logs_nothing() {
        let out = crate::testkit::CaptureWriter::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            let _ = crate::mcp::secure_tenant_router();
        });
        assert_eq!(out.contents(), "");
    }

    /// #1962 — a minted JWT ends at a secret rotation, and its grants
    /// follow the rows, not the claims.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn a_jwt_ends_at_rotation_and_reads_live_grants() {
        use crate::tenancy::{
            create_agent_pool, create_skill_pool, grant_skill_pool, revoke_skill_pool,
            rotate_agent_secret_pool,
        };
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::migrate_framework(&pool).await.unwrap();
        create_skill_pool(&pool, "editor", "Editor", "", "", &["edit".into()])
            .await
            .unwrap();
        let bot = create_agent_pool(&pool, "bot").await.unwrap();
        grant_skill_pool(&pool, "acme", "bot", "editor")
            .await
            .unwrap();
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());
        let Ok(minted) = mint_agent_jwt(
            &jwt,
            &pool,
            &crate::tenancy::TenantSlug::new(1, "acme"),
            "bot",
            &bot.token,
            &ClientIp::default(),
        )
        .await
        else {
            panic!("mint");
        };
        let tools = || async {
            authenticate_bearer(
                &jwt,
                &pool,
                &crate::tenancy::TenantSlug::new(1, "acme"),
                &minted.token,
                &ClientIp::default(),
            )
            .await
            .ok()
            .map(|a| a.tools)
        };
        assert_eq!(tools().await, Some(vec!["edit".to_owned()]));
        revoke_skill_pool(&pool, "acme", "bot", "editor")
            .await
            .unwrap();
        assert_eq!(tools().await, Some(vec![]), "a revoked skill ends at once");
        rotate_agent_secret_pool(&pool, "bot").await.unwrap();
        assert_eq!(tools().await, None, "a rotation ends the JWT");
    }

    /// A user-owned JWT reads the agent and owner rows once per request.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn a_user_key_jwt_reads_each_row_once() {
        use crate::tenancy::{
            create_skill_pool, create_user_key_pool, map_skill_to_permission_pool,
        };
        // A file, not `:memory:`: every pooled connection must see one database.
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("t.db").display());
        let pool = crate::sql::Pool::connect(&url).await.unwrap();
        crate::testkit::migrate_framework(&pool).await.unwrap();
        crate::sql::raw_execute_pool(
            &pool,
            "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
             VALUES ('carol', '', 0, 1, datetime('now'))",
            Vec::new(),
        )
        .await
        .unwrap();
        let uid = 1;
        crate::tenancy::permissions::set_user_perm_pool(uid, "app.edit", true, &pool)
            .await
            .unwrap();
        create_skill_pool(&pool, "editor", "Editor", "", "", &["edit".into()])
            .await
            .unwrap();
        map_skill_to_permission_pool(&pool, "editor", "app.edit")
            .await
            .unwrap();
        let key = create_user_key_pool(&pool, uid, "laptop", &[])
            .await
            .unwrap();
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());
        let ip = ClientIp::default();
        let minted = match mint_agent_jwt(
            &jwt,
            &pool,
            &crate::tenancy::TenantSlug::new(1, "acme"),
            &key.agent.name,
            &key.token,
            &ip,
        )
        .await
        {
            Ok(m) => m,
            Err(MintError::Unauthorized) => panic!("mint: unauthorized"),
            Err(MintError::Internal) => panic!("mint: internal"),
            Err(MintError::Refused(_)) => panic!("mint: refused"),
        };
        // Agent, owner, permissions, skill mappings, pinned grants, skills with tools.
        let agent = crate::test_assertions::assert_num_queries(
            6,
            authenticate_bearer(
                &jwt,
                &pool,
                &crate::tenancy::TenantSlug::new(1, "acme"),
                &minted.token,
                &ip,
            ),
        )
        .await
        .ok()
        .expect("authenticated");
        assert_eq!(agent.tools, vec!["edit".to_owned()]);
    }

    /// A secret `SessionSecret` refuses is flagged; a valid one keeps the
    /// key existing agent tokens were signed with.
    #[test]
    fn the_agent_key_follows_the_session_secret_rules() {
        use base64::Engine as _;
        let b64 = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![7u8; n]);
        let good = b64(32);
        assert!(matches!(agent_key(Some(&good)), AgentKey::Valid(k) if k == good.as_bytes()));
        let pass = "correct horse battery staple and two more";
        assert!(matches!(agent_key(Some(pass)), AgentKey::Unusable(_)));
        // 32 characters of base64, but only 24 bytes of key.
        assert!(matches!(agent_key(Some(&b64(24))), AgentKey::Unusable(..)));
        assert!(matches!(agent_key(Some("short")), AgentKey::Missing));
        assert!(matches!(agent_key(None), AgentKey::Missing));
    }

    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    fn client(ip: [u8; 4]) -> ClientIp {
        let mut ext = no_ext();
        ext.insert(crate::real_ip::TrustedRealIp(ip.into()));
        ClientIp::from_parts(&ext, &HeaderMap::new())
    }

    /// Wrong agent secrets are limited per IP, on the token endpoints and
    /// as a raw bearer, so a flood cannot fill the hashing queue.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn wrong_agent_secrets_are_limited_per_ip() {
        let _g = super::raw_key_cache_test_lock().lock().await;
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::migrate_framework(&pool).await.unwrap();
        let bot = crate::tenancy::create_agent_pool(&pool, "bot")
            .await
            .unwrap();
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());
        // The bucket refills while Argon2 runs, so allow a few extra tries.
        let tries = crate::login_throttle::shared().limits().ip_limit + 5;
        let throttled = |r: LoginRefused| matches!(r, LoginRefused::Throttled { .. });

        let (slug, ip) = (&TenantSlug::new(1, "t-mint-limit"), client([10, 78, 0, 1]));
        let mut refused = false;
        for _ in 0..tries {
            match mint_agent_jwt(&jwt, &pool, slug, "bot", "wrong", &ip).await {
                Err(MintError::Unauthorized) => {}
                Err(MintError::Refused(r)) if throttled(r) => refused = true,
                _ => panic!("unexpected mint outcome"),
            }
            if refused {
                break;
            }
        }
        assert!(refused, "wrong secrets were never limited");
        let r = mint_agent_jwt(&jwt, &pool, slug, "bot", &bot.token, &ip).await;
        assert!(matches!(
            r,
            Err(MintError::Refused(LoginRefused::Throttled { .. }))
        ));
        let other = client([10, 78, 0, 2]);
        assert!(mint_agent_jwt(&jwt, &pool, slug, "bot", &bot.token, &other)
            .await
            .is_ok());

        let (slug, ip) = (&TenantSlug::new(1, "t-raw-limit"), client([10, 78, 1, 1]));
        let junk = "abcdef12.0123456789abcdef";
        let mut refused = false;
        for _ in 0..tries {
            match authenticate_bearer(&jwt, &pool, slug, junk, &ip).await {
                Err(BearerRejection::Unauthorized) => {}
                Err(BearerRejection::Refused(r)) if throttled(r) => refused = true,
                _ => panic!("unexpected bearer outcome"),
            }
            if refused {
                break;
            }
        }
        assert!(refused, "wrong raw keys were never limited");
        let r = authenticate_bearer(&jwt, &pool, slug, &bot.token, &ip).await;
        assert!(matches!(
            r,
            Err(BearerRejection::Refused(LoginRefused::Throttled { .. }))
        ));
    }

    #[tokio::test]
    async fn token_round_trips_the_owning_user_id() {
        use crate::tenancy::jwt_lifecycle::JwtLifecycle;
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());

        // A user-owned key carries `uid`.
        let token = issue_agent_token(
            &jwt,
            5,
            &crate::tenancy::TenantSlug::new(1, "acme"),
            &["coach".into()],
            &["log".into()],
            Some(99),
            "abcd1234",
        )
        .expect("issue");
        let agent = verify_agent_token(&jwt, &token, &crate::tenancy::TenantSlug::new(1, "acme"))
            .await
            .expect("verify");
        assert_eq!(agent.agent_id, 5);
        assert_eq!(agent.user_id, Some(99));
        assert_eq!(agent.tools, vec!["log"]);

        // A machine agent has no `uid`.
        let token = issue_agent_token(
            &jwt,
            5,
            &crate::tenancy::TenantSlug::new(1, "acme"),
            &[],
            &[],
            None,
            "abcd1234",
        )
        .expect("issue");
        assert_eq!(
            verify_agent_token(&jwt, &token, &crate::tenancy::TenantSlug::new(1, "acme"))
                .await
                .expect("verify")
                .user_id,
            None
        );
    }

    /// A tenant that reuses a purged tenant's slug refuses its agent tokens,
    /// and a token with no `org_id` is refused anywhere.
    #[tokio::test]
    async fn an_agent_token_is_bound_to_the_org_id() {
        use crate::tenancy::jwt_lifecycle::JwtLifecycle;
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());
        let (old, heir) = (TenantSlug::new(1, "acme"), TenantSlug::new(2, "acme"));
        let token = issue_agent_token(&jwt, 5, &old, &[], &[], None, "abcd1234").unwrap();
        assert!(verify_agent_token(&jwt, &token, &old).await.is_some());
        assert!(verify_agent_token(&jwt, &token, &heir).await.is_none());

        let mut custom = serde_json::Map::new();
        custom.insert(CLAIM_KIND.into(), json!(KIND_AGENT));
        custom.insert(CLAIM_KEY_PREFIX.into(), json!("abcd1234"));
        custom.insert(CLAIM_TENANT.into(), json!("acme"));
        let unbound = jwt.issue_access_with(5, custom).unwrap();
        assert!(verify_agent_token(&jwt, &unbound, &old).await.is_none());
    }

    fn headers(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, host.parse().unwrap());
        h
    }

    fn no_ext() -> axum::http::Extensions {
        axum::http::Extensions::new()
    }

    /// `X-Forwarded-Proto` counts only from a trusted proxy.
    #[test]
    fn forwarded_proto_needs_a_trusted_proxy() {
        let mut h = headers("app.example");
        h.insert("x-forwarded-proto", "http".parse().unwrap());
        assert_eq!(origin(&h, &no_ext()), "https://app.example");
        let mut ext = no_ext();
        ext.insert(crate::real_ip::TrustedRealIp([10, 0, 0, 1].into()));
        assert_eq!(origin(&h, &ext), "http://app.example");
    }

    /// Loopback dev hosts advertise http unless a trusted proxy says https.
    #[test]
    fn loopback_hosts_advertise_http() {
        for host in [
            "acme.localhost:8080",
            "localhost",
            "127.0.0.1:8000",
            "[::1]:8080",
        ] {
            assert_eq!(origin(&headers(host), &no_ext()), format!("http://{host}"));
        }
        assert_eq!(
            origin(&headers("example.com"), &no_ext()),
            "https://example.com"
        );
        assert_eq!(
            origin(&headers("localhost.example.com"), &no_ext()),
            "https://localhost.example.com"
        );
        let mut h = headers("acme.localhost:8080");
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        let mut ext = no_ext();
        ext.insert(crate::real_ip::TrustedRealIp([10, 0, 0, 1].into()));
        assert_eq!(origin(&h, &ext), "https://acme.localhost:8080");
    }

    /// A Host with userinfo never reaches the discovery URLs (#1963).
    #[test]
    fn origin_ignores_a_host_with_userinfo() {
        for host in ["app.example:1@evil.com:2", "app.example@evil.com"] {
            assert_eq!(
                origin(&headers(host), &no_ext()),
                "http://localhost",
                "{host}"
            );
        }
    }

    /// A busy raw-key check is 503 + Retry-After, through the same
    /// conversion `authenticate_bearer` uses (#1748).
    #[test]
    fn a_busy_raw_key_check_is_503_not_401() {
        use crate::tenancy::{AgentError, TenancyError};
        let uri: Uri = "/mcp".parse().unwrap();
        let r = BearerRejection::from(AgentError::Tenancy(TenancyError::Busy)).into_response(
            &headers("app.example"),
            &no_ext(),
            &uri,
        );
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
    }

    /// A raw-key check that errors (here: no agent table) is a 500,
    /// not a 401 that sends the client off on an OAuth flow (#1748).
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_failed_raw_key_check_is_500_not_401() {
        use crate::tenancy::jwt_lifecycle::JwtLifecycle;
        let jwt = JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec());
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let Err(e) = authenticate_bearer(
            &jwt,
            &pool,
            &crate::tenancy::TenantSlug::new(1, "acme"),
            "abcdef12.0123456789abcdef",
            &ClientIp::default(),
        )
        .await
        else {
            panic!("a failed check must not authenticate");
        };
        let uri: Uri = "/mcp".parse().unwrap();
        let r = e.into_response(&headers("app.example"), &no_ext(), &uri);
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn mount_base_tracks_the_nest_prefix() {
        let h = headers("app.example");
        // On the JSON-RPC endpoint the path already is the prefix.
        let uri: Uri = "/mcp".parse().unwrap();
        assert_eq!(
            mount_base(&h, &no_ext(), &uri, ""),
            "https://app.example/mcp"
        );
        // On a discovery document, strip the suffix off to get it.
        let uri: Uri = "/api/mcp/.well-known/oauth-protected-resource"
            .parse()
            .unwrap();
        assert_eq!(
            mount_base(&h, &no_ext(), &uri, "/.well-known/oauth-protected-resource"),
            "https://app.example/api/mcp"
        );
        // A mount at the origin root gives an empty prefix, with no
        // trailing slash left behind.
        let uri: Uri = "/.well-known/oauth-protected-resource".parse().unwrap();
        assert_eq!(
            mount_base(&h, &no_ext(), &uri, "/.well-known/oauth-protected-resource"),
            "https://app.example"
        );
    }

    #[test]
    fn unauthorized_challenge_points_at_the_mount_prefix() {
        let h = headers("app.example");
        let uri: Uri = "/mcp".parse().unwrap();
        let resp = unauthorized(&h, &no_ext(), &uri);
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
        assert_eq!(
            mount_base(&h, &no_ext(), &uri, ""),
            "http://localhost:8080/mcp"
        );
    }

    /// The raw-key cache is shared with the transport tests, so all take turns.
    fn cache_lock() -> tokio::sync::MutexGuard<'static, ()> {
        super::raw_key_cache_test_lock().blocking_lock()
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

    /// #2301 — a full cache drops its oldest entry, not every entry.
    #[test]
    fn a_full_cache_evicts_only_the_oldest() {
        let _g = cache_lock();
        raw_key_cache().lock().unwrap().clear();
        let keys: Vec<_> = (0..=RAW_KEY_CACHE_CAP)
            .map(|i| raw_key_cache_key("acme", &format!("pfx.{i}")))
            .collect();
        for (i, key) in keys.iter().enumerate() {
            raw_key_cache_put(*key, i as i64);
        }
        assert_eq!(raw_key_cache_get(&keys[0]), None, "oldest stays cached");
        for (i, key) in keys.iter().enumerate().skip(1) {
            assert_eq!(raw_key_cache_get(key), Some(i as i64), "entry {i} evicted");
        }
        // Re-putting a cached key at the cap evicts nothing.
        let newest = keys.len() - 1;
        raw_key_cache_put(keys[newest], newest as i64);
        assert_eq!(
            raw_key_cache_get(&keys[1]),
            Some(1),
            "re-put evicted the oldest"
        );
        raw_key_cache().lock().unwrap().clear();
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
