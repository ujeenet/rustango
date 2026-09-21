//! Auth signals, in Django's shape: `user_logged_in`,
//! `user_logged_out` and `user_login_failed`.
//!
//! These are lifecycle events, not row events, so receivers register
//! globally and run one at a time, in registration order. The
//! framework's own login, logout and failed-login paths send them.
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
//! ## When each one is sent
//!
//! - `user_logged_in`: after any successful login, on any surface.
//!   [`UserLoggedInContext`] carries the user id, username,
//!   `is_superuser`, and a `source` tag naming the login path, so one
//!   receiver can handle several surfaces and still tell them apart.
//! - `user_logged_out`: on any logout. `user_id` and `username` are
//!   optional, because some logout endpoints run with no session, for
//!   example on a stale-cookie probe.
//! - `user_login_failed`: on a wrong credential or an inactive
//!   account. `attempted_username` is `None` when the form was too
//!   malformed to read it.
//!
//! Receivers run one at a time, in registration order, each with its
//! own clone of the context. To run work in parallel, or to keep a
//! panic from stopping the chain, use `tokio::spawn`.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// The future a receiver returns. It is `'static` because the
/// receiver is stored behind an `Arc` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Handle returned by `connect_*`, for a later `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

// ---------------------------------------------------------------- Context types

/// Request details every auth context carries: origin IP, user
/// agent and path. All are optional, because which one is available
/// depends on the layers that ran before the auth handler. Audit
/// receivers usually copy them straight into their record.
#[derive(Debug, Clone, Default)]
pub struct AuthRequestMeta {
    /// Origin IP, from the `real_ip` middleware when it ran, else
    /// the peer socket address.
    pub ip_address: Option<String>,
    /// The `User-Agent` header.
    pub user_agent: Option<String>,
    /// Request path with its query string, for debugging and audit.
    pub path: Option<String>,
}

/// What a `user_logged_in` receiver gets.
#[derive(Debug, Clone)]
pub struct UserLoggedInContext {
    /// Which login surface this was: `"admin"`, `"tenant_admin"`,
    /// `"operator"`, `"jwt"` and so on. It lets one receiver cover
    /// several paths and still tell them apart.
    pub source: &'static str,
    /// Id of the user who just signed in.
    pub user_id: i64,
    /// The login identifier for this surface.
    pub username: String,
    /// Whether that user is a superuser.
    pub is_superuser: bool,
    /// See [`AuthRequestMeta`].
    pub request: AuthRequestMeta,
}

/// What a `user_logged_out` receiver gets.
///
/// `user_id` and `username` are optional, because a logout endpoint
/// can run with no session, for example on a stale-cookie probe.
/// Filter on `user_id.is_some()` for real logouts only.
#[derive(Debug, Clone)]
pub struct UserLoggedOutContext {
    pub source: &'static str,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub request: AuthRequestMeta,
}

/// Why a login attempt was rejected. Receivers use it to separate a
/// credential-stuffing alert
/// ([`AuthFailureReason::InvalidCredentials`]) from an ordinary
/// rejection ([`AuthFailureReason::Inactive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthFailureReason {
    /// No such username, or the password did not verify.
    InvalidCredentials,
    /// The user exists but the account is disabled.
    Inactive,
    /// Lockout, rate limit or captcha. The exact rule is up to the
    /// surface.
    Locked,
    /// Anything else. Framework code uses the variants above, so
    /// this one is rare.
    Other,
}

/// What a `user_login_failed` receiver gets.
#[derive(Debug, Clone)]
pub struct UserLoginFailedContext {
    pub source: &'static str,
    /// The username submitted. `None` when the form was too
    /// malformed to read it, which is rare.
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

/// Register a `user_logged_in` receiver. It runs after a successful
/// login on any of the framework's surfaces.
pub fn connect_user_logged_in<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(UserLoggedInContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: LoggedInReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::LoggedIn, boxed)
}

/// Remove a `user_logged_in` receiver. `true` when one was removed.
pub fn disconnect_user_logged_in(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::LoggedIn, id)
}

/// Send `user_logged_in`, awaiting each receiver in registration
/// order.
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

/// Remove a `user_logged_out` receiver.
pub fn disconnect_user_logged_out(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::LoggedOut, id)
}

/// Send `user_logged_out` for `ctx`.
pub async fn send_user_logged_out(ctx: UserLoggedOutContext) {
    let receivers: Vec<LoggedOutReceiver> = snapshot(SignalKind::LoggedOut);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- user_login_failed

/// Register a `user_login_failed` receiver. It runs on every
/// rejected attempt: unknown username, wrong password, inactive
/// account and so on. See [`AuthFailureReason`].
pub fn connect_user_login_failed<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(UserLoginFailedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: LoginFailedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::LoginFailed, boxed)
}

/// Remove a `user_login_failed` receiver.
pub fn disconnect_user_login_failed(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::LoginFailed, id)
}

/// Send `user_login_failed` for `ctx`.
pub async fn send_user_login_failed(ctx: UserLoginFailedContext) {
    let receivers: Vec<LoginFailedReceiver> = snapshot(SignalKind::LoginFailed);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Helpers

/// Read what [`AuthRequestMeta`] it can from request headers:
/// `User-Agent`, plus the `X-Real-IP` and `X-Forwarded-For` chain the
/// `real_ip` middleware sets. With none of them present it returns
/// all-`None`, so a caller can always fill `request:` unconditionally.
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

/// Remove every auth-signal receiver. Mostly for resetting state
/// between tests.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// How many receivers are registered across the three auth signals.
/// Mostly useful in tests.
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
