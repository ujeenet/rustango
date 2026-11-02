//! The Streamable HTTP transport. `POST {prefix}` carries one
//! JSON-RPC message from the client. `GET {prefix}` opens an SSE
//! stream that carries `progress` and `list_changed` notifications
//! back.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::tenancy::jwt_lifecycle::JwtLifecycle;

use super::handlers::dispatch;
use super::router::{AuthedMcpState, McpState};
use super::types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

/// `POST {prefix}`: parse one JSON-RPC message, run it, reply.
///
/// - Malformed JSON gives a `parse error` with a null id.
/// - Valid JSON in the wrong shape gives an `invalid request`.
/// - A notification, which has no `id`, gets `202 Accepted` and an
///   empty body.
/// - A request gets a `200` with a success or error result.
pub(crate) async fn post_handler(State(state): State<McpState>, body: Bytes) -> Response {
    // This route does not authenticate, so there is no agent and the
    // `tools/*` methods are refused.
    handle_message(&state, &body, None).await
}

/// Parse one message, dispatch it, and build the HTTP response.
/// Shared with the authenticated handler in [`super::auth`], which
/// verifies the token first and passes the agent context in.
pub(crate) async fn handle_message(
    state: &McpState,
    body: &[u8],
    ctx: Option<super::tools::McpContext>,
) -> Response {
    // Parse in two steps, so a message with valid JSON but the wrong
    // shape still yields its `id` for the error response.
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return json_error(Value::Null, JsonRpcError::parse_error()),
    };
    let recovered_id = value.get("id").cloned().unwrap_or(Value::Null);
    // MCP ids are never null; serde would read one as a notification (#1963).
    let null_id = value.get("id").is_some_and(Value::is_null);
    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => return json_error(recovered_id, JsonRpcError::invalid_request(e.to_string())),
    };
    if request.jsonrpc != super::types::JSONRPC_VERSION {
        return json_error(
            recovered_id,
            JsonRpcError::invalid_request("jsonrpc must be \"2.0\""),
        );
    }
    if null_id {
        return json_error(
            Value::Null,
            JsonRpcError::invalid_request("id must not be null"),
        );
    }

    if request.is_notification() {
        // `notifications/cancelled { requestId }` trips the cancel
        // token of a call in flight. It is scoped to the agent that
        // sent it, so nobody can cancel another agent's call. With no
        // authenticated agent there is nothing to cancel. Every other
        // notification, such as `initialized`, does nothing.
        if request.method == "notifications/cancelled" {
            if let (Some(ctx), Some(rid)) = (
                ctx.as_ref(),
                request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("requestId"))
                    .map(jsonrpc_id_string),
            ) {
                super::progress::cancel(&ctx.agent.tenant, ctx.agent.agent_id, &rid);
            }
        }
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    let request_id = jsonrpc_id_string(&id);
    match dispatch(
        state,
        &request.method,
        request.params,
        ctx,
        Some(request_id.as_str()),
    )
    .await
    {
        Ok(result) => Json(JsonRpcResponse::success(id, result)).into_response(),
        Err(err) => Json(JsonRpcResponse::failure(id, err)).into_response(),
    }
}

/// `GET {prefix}`: an SSE stream of notifications **for the
/// connected agent only**.
///
/// It needs the same agent bearer token as the JSON-RPC endpoint, and
/// then filters the shared bus, so an agent never sees another
/// agent's or another tenant's frames. It ends at the JWT's `exp`, or
/// within a minute of a revoke.
pub(crate) fn sse_handler<DB: crate::sql::sqlx::Database>(
    t: crate::extractors::Tenant<DB>,
    axum::extract::State(state): axum::extract::State<AuthedMcpState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: axum::http::HeaderMap,
    extensions: axum::http::Extensions,
) -> impl std::future::Future<Output = Response> + Send {
    let t = t.into();
    async move { sse_in(t, state.jwt, uri, headers, extensions).await }
}

async fn sse_in(
    t: crate::extractors::TenantScope,
    jwt: std::sync::Arc<JwtLifecycle>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    extensions: axum::http::Extensions,
) -> Response {
    let Some(token) = super::auth::bearer(&headers) else {
        return super::auth::unauthorized(&headers, &extensions, &uri);
    };
    // Accept both bearer shapes the JSON-RPC POST accepts. See
    // `auth::authenticate_bearer` for why this stream must not be
    // stricter than the endpoint next to it.
    let ip = crate::login_throttle::ClientIp::from_parts(&extensions, &headers);
    let Some(tenant) = crate::tenancy::TenantSlug::of(&t.org) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "tenant has no id").into_response();
    };
    let authed = super::auth::authenticate_bearer_until(&jwt, t.pool(), &tenant, token, &ip);
    let (agent, exp) = match authed.await {
        Ok(v) => v,
        Err(e) => return e.into_response(&headers, &extensions, &uri),
    };
    open_capped(
        t.pool().clone(),
        agent,
        exp.map(|exp| JwtBound { jwt, exp }),
        StreamCaps::from_env(),
        crate::shutdown::Shutdown::from_extensions(&extensions),
    )
}

/// The agent's stream, or `429` when it or its tenant has too many open.
fn open_capped(
    pool: crate::sql::Pool,
    agent: super::McpAgent,
    token: Option<JwtBound>,
    caps: StreamCaps,
    shutdown: crate::shutdown::Shutdown,
) -> Response {
    let Some(slot) = StreamSlot::acquire(&agent.tenant, agent.agent_id, caps) else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many open notification streams",
        )
            .into_response();
    };
    agent_sse(
        pool,
        agent,
        token,
        KEEP_ALIVE * RECHECK_EVERY_KEEP_ALIVES,
        slot,
        shutdown,
    )
}

/// How many streams may be open at once in this process; `0` is no cap.
#[derive(Clone, Copy, Debug)]
struct StreamCaps {
    per_agent: usize,
    per_tenant: usize,
}

impl StreamCaps {
    const PER_AGENT_VAR: &'static str = "RUSTANGO_MCP_SSE_PER_AGENT";
    const PER_TENANT_VAR: &'static str = "RUSTANGO_MCP_SSE_PER_TENANT";

    /// The env overrides, else 8 per agent and 256 per tenant.
    fn from_env() -> Self {
        let read = |var: &str, default: usize| {
            std::env::var(var)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        Self {
            per_agent: read(Self::PER_AGENT_VAR, 8),
            per_tenant: read(Self::PER_TENANT_VAR, 256),
        }
    }
}

/// Open streams by `(tenant, agent)` and by tenant. Keyed by tenant, so
/// one tenant's count never touches another's.
#[derive(Default)]
struct OpenStreams {
    by_agent: std::collections::HashMap<(String, i64), usize>,
    by_tenant: std::collections::HashMap<String, usize>,
}

fn open_streams() -> std::sync::MutexGuard<'static, OpenStreams> {
    static OPEN: std::sync::OnceLock<std::sync::Mutex<OpenStreams>> = std::sync::OnceLock::new();
    OPEN.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One open stream's place under the caps; dropping it frees the place.
struct StreamSlot {
    tenant: String,
    agent_id: i64,
}

impl StreamSlot {
    fn acquire(tenant: &str, agent_id: i64, caps: StreamCaps) -> Option<Self> {
        let mut open = open_streams();
        let key = (tenant.to_owned(), agent_id);
        let agent_n = open.by_agent.get(&key).copied().unwrap_or(0);
        let tenant_n = open.by_tenant.get(tenant).copied().unwrap_or(0);
        let full = |n: usize, cap: usize| cap != 0 && n >= cap;
        if full(agent_n, caps.per_agent) || full(tenant_n, caps.per_tenant) {
            return None;
        }
        open.by_agent.insert(key, agent_n + 1);
        open.by_tenant.insert(tenant.to_owned(), tenant_n + 1);
        Some(Self {
            tenant: tenant.to_owned(),
            agent_id,
        })
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut open = open_streams();
        let key = (std::mem::take(&mut self.tenant), self.agent_id);
        if let Some(n) = open.by_agent.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                open.by_agent.remove(&key);
            }
        }
        if let Some(n) = open.by_tenant.get_mut(&key.0) {
            *n -= 1;
            if *n == 0 {
                open.by_tenant.remove(&key.0);
            }
        }
    }
}

/// A JWT stream's token: it ends at `exp` or when its JTI is revoked (#2303).
struct JwtBound {
    jwt: std::sync::Arc<JwtLifecycle>,
    exp: i64,
}

const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// The stream re-checks the agent row every 4 keep-alives (once a minute):
/// one indexed lookup per open stream, and a revoke ends it within 60s (#2237).
const RECHECK_EVERY_KEEP_ALIVES: u32 = 4;

/// The agent's frames until the JWT's `exp`, a re-check that finds the agent
/// revoked, deactivated or rotated or the JWT revoked, the bus closing, or shutdown.
fn agent_sse(
    pool: crate::sql::Pool,
    agent: super::McpAgent,
    token: Option<JwtBound>,
    recheck_every: Duration,
    slot: StreamSlot,
    shutdown: crate::shutdown::Shutdown,
) -> Response {
    use tokio::sync::broadcast::error::RecvError;
    // Own tenant's channel: another tenant's burst cannot make it lag.
    let mut rx = super::notifications::subscribe(&agent.tenant);
    let exp = token.as_ref().map(|t| t.exp);
    let stream = async_stream::stream! {
        // Held while the stream lives; the client going away frees it.
        let _slot = slot;
        let expired = async move {
            match exp {
                Some(exp) => {
                    let left = exp.saturating_mul(1000) - chrono::Utc::now().timestamp_millis();
                    tokio::time::sleep(Duration::from_millis(u64::try_from(left).unwrap_or(0))).await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::pin!(expired);
        let start = tokio::time::Instant::now() + recheck_every;
        let mut recheck = tokio::time::interval_at(start, recheck_every);
        loop {
            let received = tokio::select! {
                () = &mut expired => break,
                _ = recheck.tick() => {
                    if let Some(t) = &token {
                        if t.jwt.is_blacklisted(&agent.jti).await {
                            break;
                        }
                    }
                    let live = crate::tenancy::agent_token_still_valid_pool(
                        &pool,
                        agent.agent_id,
                        agent.user_id,
                        &agent.secret_prefix,
                    )
                    .await;
                    match live {
                        Ok(true) => {}
                        Ok(false) => break,
                        // A DB blip must not drop every open stream: retry next tick.
                        Err(e) => tracing::warn!(error = %e, "mcp sse liveness re-check failed"),
                    }
                    continue;
                }
                r = rx.recv() => r,
            };
            match received {
                Ok(frame) => {
                    if super::notifications::frame_visible(&frame, &agent.tenant, agent.agent_id) {
                        yield Ok::<Event, std::convert::Infallible>(Event::default().data(frame.body));
                    }
                    // Frames for anyone else are dropped.
                }
                // This client fell behind the buffer. Skip what it
                // missed rather than closing the connection.
                Err(RecvError::Lagged(_)) => continue,
                // Every sender is gone, so end the stream.
                Err(RecvError::Closed) => break,
            }
        }
    };
    Sse::new(shutdown.cut(stream))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

fn json_error(id: Value, error: JsonRpcError) -> Response {
    Json(JsonRpcResponse::failure(id, error)).into_response()
}

/// Turn a JSON-RPC id, which may be a string or a number, into one
/// stable key, so a `cancelled` notification finds its call.
fn jsonrpc_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(all(test, feature = "sqlite", feature = "testkit"))]
mod tests {
    use super::*;
    use crate::shutdown::Shutdown;
    use crate::tenancy::{create_agent_pool, rotate_agent_secret_pool};

    type World = (
        crate::sql::Pool,
        super::super::McpAgent,
        Option<JwtBound>,
        String,
    );

    /// A pool with agent `bot`, authenticated by a JWT from `jwt`, or by its raw key.
    async fn world(jwt: JwtLifecycle, raw_key: bool) -> World {
        let jwt = std::sync::Arc::new(jwt);
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::migrate_framework(&pool).await.unwrap();
        let bot = create_agent_pool(&pool, "bot").await.unwrap();
        let ip = crate::login_throttle::ClientIp::default();
        let bearer = if raw_key {
            bot.token
        } else {
            let Ok(minted) = super::super::auth::mint_agent_jwt(
                &jwt,
                &pool,
                &crate::tenancy::TenantSlug::new(1, "acme"),
                "bot",
                &bot.token,
                &ip,
            )
            .await
            else {
                panic!("mint");
            };
            minted.token
        };
        let Ok((agent, exp)) = super::super::auth::authenticate_bearer_until(
            &jwt,
            &pool,
            &crate::tenancy::TenantSlug::new(1, "acme"),
            &bearer,
            &ip,
        )
        .await
        else {
            panic!("authenticate");
        };
        (pool, agent, exp.map(|exp| JwtBound { jwt, exp }), bearer)
    }

    fn jwt() -> JwtLifecycle {
        JwtLifecycle::new(b"unit-secret-at-least-32-bytes-long!!".to_vec())
    }

    const NO_CAP: StreamCaps = StreamCaps {
        per_agent: 0,
        per_tenant: 0,
    };

    /// `agent_sse` with an uncapped slot.
    fn open(
        pool: crate::sql::Pool,
        agent: super::super::McpAgent,
        token: Option<JwtBound>,
        recheck: Duration,
    ) -> Response {
        let slot = StreamSlot::acquire(&agent.tenant, agent.agent_id, NO_CAP).unwrap();
        agent_sse(pool, agent, token, recheck, slot, Shutdown::default())
    }

    /// Past either cap a stream is refused; a closed stream frees its place.
    #[test]
    fn open_streams_are_capped_per_agent_and_per_tenant() {
        let tenant = format!("cap-{}", std::process::id());
        let caps = StreamCaps {
            per_agent: 2,
            per_tenant: 3,
        };
        let a1 = StreamSlot::acquire(&tenant, 1, caps).unwrap();
        let a2 = StreamSlot::acquire(&tenant, 1, caps).unwrap();
        assert!(StreamSlot::acquire(&tenant, 1, caps).is_none(), "per agent");
        let b1 = StreamSlot::acquire(&tenant, 2, caps).unwrap();
        assert!(
            StreamSlot::acquire(&tenant, 3, caps).is_none(),
            "per tenant"
        );
        assert!(
            StreamSlot::acquire(&format!("{tenant}-other"), 1, caps).is_some(),
            "another tenant has its own count"
        );
        drop(a1);
        let again = StreamSlot::acquire(&tenant, 1, caps);
        assert!(again.is_some(), "a dropped stream frees its place");
        drop((a2, b1, again));
    }

    /// The handler answers 429 past the per-agent cap.
    #[tokio::test]
    async fn the_handler_refuses_past_the_cap() {
        let (pool, mut agent, _, _) = world(jwt(), false).await;
        // Its own agent id: parallel tests hold streams for the shared one.
        agent.agent_id = i64::MAX - 443;
        let caps = StreamCaps {
            per_agent: 1,
            per_tenant: 0,
        };
        let held = StreamSlot::acquire(&agent.tenant, agent.agent_id, caps).unwrap();
        let resp = open_capped(pool, agent, None, caps, Shutdown::default());
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(held);
    }

    /// Open a stream re-checking every 50ms, check it stays open, revoke, expect it to end.
    async fn assert_ends_on_revoke(
        pool: crate::sql::Pool,
        agent: super::super::McpAgent,
        token: Option<JwtBound>,
    ) {
        let body = drain(open(pool.clone(), agent, token, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!body.is_finished(), "a live agent's stream must stay open");
        rotate_agent_secret_pool(&pool, "bot").await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), body).await;
        assert!(ended.is_ok(), "the stream outlived the revoke");
    }

    fn drain(resp: Response) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
        })
    }

    /// #2237 — the stream ends at the JWT's `exp`.
    #[tokio::test]
    async fn the_stream_ends_when_the_jwt_expires() {
        let (pool, agent, token, _) = world(jwt().with_access_ttl(2), false).await;
        assert!(token.is_some());
        let body = drain(open(pool, agent, token, Duration::from_secs(3600)));
        let ended = tokio::time::timeout(Duration::from_secs(6), body).await;
        assert!(ended.is_ok(), "the stream outlived its token");
    }

    /// #2237 — a rotated (revoked) agent's stream ends at the next re-check.
    #[tokio::test]
    async fn the_stream_ends_when_the_agent_is_revoked() {
        let (pool, agent, token, _) = world(jwt(), false).await;
        assert_ends_on_revoke(pool, agent, token).await;
    }

    /// #2303 — revoking the JWT itself ends its stream at the next re-check.
    #[tokio::test]
    async fn the_stream_ends_when_the_jwt_is_revoked() {
        let (pool, agent, token, bearer) = world(jwt(), false).await;
        let jwt = token.as_ref().map(|t| t.jwt.clone()).unwrap();
        let body = drain(open(pool, agent, token, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!body.is_finished(), "a live token's stream must stay open");
        assert!(jwt.revoke(&bearer).await);
        let ended = tokio::time::timeout(Duration::from_secs(5), body).await;
        assert!(ended.is_ok(), "the stream outlived the JWT revoke");
    }

    /// The stream ends at the server's stop signal, not at the drain deadline.
    #[tokio::test]
    async fn the_stream_ends_at_shutdown() {
        let (pool, agent, token, _) = world(jwt(), false).await;
        let (stop, on_stop) = tokio::sync::watch::channel(());
        let slot = StreamSlot::acquire(&agent.tenant, agent.agent_id, NO_CAP).unwrap();
        let resp = agent_sse(
            pool,
            agent,
            token,
            Duration::from_secs(3600),
            slot,
            Shutdown::on(on_stop),
        );
        let body = drain(resp);
        stop.send(()).unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), body).await;
        assert!(ended.is_ok(), "the stream outlived the stop signal");
    }

    /// A raw-key stream has no `exp`; the re-check alone ends it.
    #[tokio::test]
    async fn a_raw_key_stream_ends_when_the_agent_is_revoked() {
        // The raw-key path writes the shared verify cache.
        let _g = super::super::auth::raw_key_cache_test_lock().lock().await;
        let (pool, agent, token, _) = world(jwt(), true).await;
        assert!(token.is_none());
        assert_ends_on_revoke(pool, agent, token).await;
    }

    /// Another tenant's burst, past the buffer, does not drop this tenant's frame.
    #[tokio::test]
    async fn another_tenants_burst_does_not_drop_this_tenants_frames() {
        use http_body_util::BodyExt as _;
        let (pool, agent, token, _) = world(jwt(), false).await;
        let tenant = agent.tenant.clone();
        let mut body = open(pool, agent, token, Duration::from_secs(3600)).into_body();
        super::super::notifications::notify_resources_list_changed(&tenant, None);
        for _ in 0..1000 {
            super::super::notifications::notify_tools_list_changed("other-tenant", None);
        }
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(frame)) = body.frame().await {
                let data = frame.into_data().unwrap_or_default();
                if String::from_utf8_lossy(&data).contains("resources/list_changed") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(found, Ok(true), "this tenant's frame was lost");
    }

    /// A failing re-check keeps the stream open; a later revoke still ends it.
    #[tokio::test]
    async fn a_failing_recheck_keeps_the_stream_open() {
        let (pool, agent, token, _) = world(jwt(), false).await;
        // Fault injection: hide the agents table so the re-check errors.
        let rename = |from: &'static str, to: &'static str| {
            let pool = pool.clone();
            async move {
                crate::sql::raw_execute_pool(
                    &pool,
                    &format!("ALTER TABLE {from} RENAME TO {to}"),
                    vec![],
                )
                .await
                .unwrap();
            }
        };
        rename("rustango_agents", "rustango_agents_off").await;
        let body = drain(open(pool.clone(), agent, token, Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!body.is_finished(), "a DB error must not end the stream");
        rename("rustango_agents_off", "rustango_agents").await;
        rotate_agent_secret_pool(&pool, "bot").await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), body).await;
        assert!(ended.is_ok(), "the stream outlived the revoke");
    }
}
