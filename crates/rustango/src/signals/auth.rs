//! Django-shape auth lifecycle signals — `user_logged_in`,
//! `user_logged_out`, `user_login_failed`. Django-parity #414.
//!
//! Receivers register globally (no per-model dispatch — these are
//! lifecycle events, not row events) and run sequentially in
//! registration order. Each signal fires from inside the framework's
//! login / logout / failed-login paths.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signals::auth::{
//!     connect_user_logged_in, connect_user_login_failed,
//!     UserLoggedInContext, UserLoginFailedContext,
//! };
//!
//! connect_user_logged_in(|ctx| Box::pin(async move {
//!     tracing::info!(user_id = ctx.user_id, source = ctx.source, "login ok");
//! }));
//! connect_user_login_failed(|ctx| Box::pin(async move {
//!     tracing::warn!(username = ?ctx.attempted_username, reason = ?ctx.reason, "login failed");
//! }));
//! ```
//!
//! ## Where each signal fires
//!
//! - `user_logged_in` — every successful login (admin POST `/login`,
//!   tenant admin login, operator-console login, …). The
//!   [`UserLoggedInContext`] carries the resolved user id, username,
//!   `is_superuser` flag, and a free-form `source` tag identifying the
//!   login path so receivers can filter by surface.
//! - `user_logged_out` — every logout path (admin POST `/logout`,
//!   tenant logout, operator logout). `user_id` and `username` are
//!   optional because some logout endpoints don't require an active
//!   session (e.g. a stale-cookie probe).
//! - `user_login_failed` — every failed credential check or
//!   inactive-account rejection. `attempted_username` is `None` when
//!   the form is so malformed we can't extract it.
//!
//! ## Semantics
//!
//! - Receivers run **sequentially** in registration order, awaited one
//!   at a time. Wrap a body in `tokio::spawn` for fanout.
//! - A panicking receiver aborts the dispatch chain and propagates;
//!   wrap in `tokio::spawn` if you need isolation.
//! - Each receiver gets a `Clone`d context — no borrow lifetimes.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Future returned by auth-signal receivers. `'static` because the
/// receiver is stored as `Arc<dyn ...>` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque identifier returned by `connect_*` for later use with
/// `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

// ---------------------------------------------------------------- Context types

/// Per-request metadata shared by every auth signal context — the
/// origin IP, user agent, and (when available) the originating path.
/// All fields are optional; the framework populates them on a
/// best-effort basis depending on which layers ran before the auth
/// handler. Audit-log receivers typically lift these straight into
/// their record.
#[derive(Debug, Clone, Default)]
pub struct AuthRequestMeta {
    /// Origin IP — resolved through `real_ip` middleware when present,
    /// otherwise the peer socket address.
    pub ip_address: Option<String>,
    /// Raw `User-Agent` header value.
    pub user_agent: Option<String>,
    /// Request path including query string. Mostly for debugging /
    /// audit; not used for routing decisions.
    pub path: Option<String>,
}

/// Payload delivered to `user_logged_in` receivers.
#[derive(Debug, Clone)]
pub struct UserLoggedInContext {
    /// Free-form identifier for the login surface — e.g. `"admin"`,
    /// `"tenant_admin"`, `"operator"`, `"jwt"`. Lets a single receiver
    /// audit across multiple login paths and tell them apart.
    pub source: &'static str,
    /// Primary-key id of the user that just authenticated.
    pub user_id: i64,
    /// Username (or whatever the framework uses as the login
    /// identifier on this surface).
    pub username: String,
    /// Whether the resolved user has framework-level superuser rights.
    pub is_superuser: bool,
    /// Origin metadata — see [`AuthRequestMeta`].
    pub request: AuthRequestMeta,
}

/// Payload delivered to `user_logged_out` receivers.
///
/// `user_id` / `username` are `Option` because logout endpoints
/// occasionally fire on a request where no session was ever
/// established (e.g. a stale-cookie probe). Audit receivers that
/// only care about real logouts should filter on `user_id.is_some()`.
#[derive(Debug, Clone)]
pub struct UserLoggedOutContext {
    pub source: &'static str,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub request: AuthRequestMeta,
}

/// Reason a credential check rejected the attempt. Receivers can
/// pivot on this to alert on credential stuffing
/// ([`AuthFailureReason::InvalidCredentials`]) vs. operational
/// rejections ([`AuthFailureReason::Inactive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthFailureReason {
    /// Username didn't exist, or password verify returned false.
    InvalidCredentials,
    /// User row exists but `active = false` (or whatever the surface
    /// uses to mark soft-disabled accounts).
    Inactive,
    /// Lockout / rate-limit / captcha — surface-specific.
    Locked,
    /// Anything else the surface wants to flag; framework code uses
    /// the named variants above so [`AuthFailureReason::Other`] is
    /// rare in practice.
    Other,
}

/// Payload delivered to `user_login_failed` receivers.
#[derive(Debug, Clone)]
pub struct UserLoginFailedContext {
    pub source: &'static str,
    /// Username submitted in the form. `None` when the form is so
    /// malformed we couldn't extract it (rare — most surfaces require
    /// the field at the schema layer).
    pub attempted_username: Option<String>,
    pub reason: AuthFailureReason,
    pub request: AuthRequestMeta,
}

// ---------------------------------------------------------------- Internal storage

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SignalKind {
    LoggedIn,
    LoggedOut,
    LoginFailed,
}

type ReceiverEntry = (ReceiverId, Box<dyn Any + Send + Sync>);
type Bag = Vec<ReceiverEntry>;

fn registry() -> &'static RwLock<HashMap<SignalKind, Bag>> {
    static REG: OnceLock<RwLock<HashMap<SignalKind, Bag>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_id() -> ReceiverId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    ReceiverId(COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn insert_receiver<R: Any + Send + Sync>(kind: SignalKind, receiver: R) -> ReceiverId {
    let id = next_id();
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.entry(kind).or_default().push((id, Box::new(receiver)));
    id
}

fn remove_receiver(kind: SignalKind, id: ReceiverId) -> bool {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get_mut(&kind) else {
        return false;
    };
    let before = bag.len();
    bag.retain(|(rid, _)| *rid != id);
    bag.len() != before
}

fn snapshot<R: Any + Send + Sync + Clone>(kind: SignalKind) -> Vec<R> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get(&kind) else {
        return Vec::new();
    };
    bag.iter()
        .filter_map(|(_, b)| b.downcast_ref::<R>().cloned())
        .collect()
}

// ---------------------------------------------------------------- Receiver type aliases

type LoggedInReceiver = Arc<dyn Fn(UserLoggedInContext) -> ReceiverFuture + Send + Sync>;
type LoggedOutReceiver = Arc<dyn Fn(UserLoggedOutContext) -> ReceiverFuture + Send + Sync>;
type LoginFailedReceiver = Arc<dyn Fn(UserLoginFailedContext) -> ReceiverFuture + Send + Sync>;

// ---------------------------------------------------------------- user_logged_in

/// Register a `user_logged_in` receiver. Fires after every successful
/// authentication on any of the framework's login surfaces.
pub fn connect_user_logged_in<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(UserLoggedInContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: LoggedInReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::LoggedIn, boxed)
}

/// Remove a previously-connected `user_logged_in` receiver. Returns
/// `true` when an entry was removed.
pub fn disconnect_user_logged_in(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::LoggedIn, id)
}

/// Fire `user_logged_in` for `ctx`. Awaits every connected receiver
/// in registration order.
pub async fn send_user_logged_in(ctx: UserLoggedInContext) {
    let receivers: Vec<LoggedInReceiver> = snapshot(SignalKind::LoggedIn);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- user_logged_out

/// Register a `user_logged_out` receiver.
pub fn connect_user_logged_out<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(UserLoggedOutContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: LoggedOutReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::LoggedOut, boxed)
}

/// Remove a previously-connected `user_logged_out` receiver.
pub fn disconnect_user_logged_out(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::LoggedOut, id)
}

/// Fire `user_logged_out` for `ctx`.
pub async fn send_user_logged_out(ctx: UserLoggedOutContext) {
    let receivers: Vec<LoggedOutReceiver> = snapshot(SignalKind::LoggedOut);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- user_login_failed

/// Register a `user_login_failed` receiver. Fires on every rejected
/// credential check — invalid username, wrong password, inactive
/// account, etc. See [`AuthFailureReason`].
pub fn connect_user_login_failed<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(UserLoginFailedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: LoginFailedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::LoginFailed, boxed)
}

/// Remove a previously-connected `user_login_failed` receiver.
pub fn disconnect_user_login_failed(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::LoginFailed, id)
}

/// Fire `user_login_failed` for `ctx`.
pub async fn send_user_login_failed(ctx: UserLoginFailedContext) {
    let receivers: Vec<LoginFailedReceiver> = snapshot(SignalKind::LoginFailed);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Helpers

/// Best-effort extraction of [`AuthRequestMeta`] from an axum request.
/// Inspects the `User-Agent` header and the `X-Real-IP` /
/// `X-Forwarded-For` chain that `real_ip` middleware sets. Returns a
/// fully-default-`None` instance when neither is present so callers can
/// always populate `request:` without conditional logic.
pub fn meta_from_headers(headers: &axum::http::HeaderMap, path: Option<&str>) -> AuthRequestMeta {
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let ip_address = header_str("x-real-ip").or_else(|| {
        header_str("x-forwarded-for").and_then(|s| s.split(',').next().map(|f| f.trim().to_owned()))
    });
    let user_agent = header_str("user-agent");
    AuthRequestMeta {
        ip_address,
        user_agent,
        path: path.map(str::to_owned),
    }
}

// ---------------------------------------------------------------- Maintenance

/// Remove **all** auth-signal receivers. Useful in tests to reset
/// registry state between cases.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Total receivers currently registered across all three auth signals.
/// Useful in tests to assert connection state.
pub fn receiver_count() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    [
        SignalKind::LoggedIn,
        SignalKind::LoggedOut,
        SignalKind::LoginFailed,
    ]
    .iter()
    .map(|kind| reg.get(kind).map_or(0, Vec::len))
    .sum()
}
