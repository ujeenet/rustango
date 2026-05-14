//! Django-shape request lifecycle signals — `request_started`,
//! `request_finished`, `got_request_exception`. Issue #53.
//!
//! Receivers register globally (not per-type — there's no `Model` here)
//! and run sequentially in registration order around every HTTP request
//! that passes through [`RequestSignalsLayer`].
//!
//! ## Quick start
//!
//! ```ignore
//! use axum::Router;
//! use rustango::signals::request::{
//!     connect_request_started, connect_request_finished,
//!     RequestSignalsLayer,
//! };
//!
//! // Register receivers at startup:
//! connect_request_started(|ctx| Box::pin(async move {
//!     tracing::info!(method = %ctx.method, path = %ctx.path, "request started");
//! }));
//! connect_request_finished(|ctx| Box::pin(async move {
//!     tracing::info!(status = ctx.status, ms = ctx.elapsed_ms, "request finished");
//! }));
//!
//! // Wire the layer onto the app — order matters: outermost layer sees
//! // the request first / response last, which is what we want for
//! // around-the-handler signals.
//! let app: Router = Router::new()
//!     // ... routes ...
//!     .layer(RequestSignalsLayer::new());
//! ```
//!
//! ## Semantics
//!
//! - Receivers run **sequentially** in registration order, awaited one
//!   at a time. For parallel fanout, wrap a body in `tokio::spawn`.
//! - A panicking receiver aborts the dispatch chain and propagates;
//!   wrap in `tokio::spawn` if you need isolation.
//! - `request_started` fires before the inner service is called.
//! - `request_finished` fires after the inner service returns a
//!   response — for every status code (2xx / 4xx / 5xx).
//! - `got_request_exception` fires when the inner service returns an
//!   error (the `S::Error` channel). Today axum services use
//!   `Infallible`, so the practical trigger is a downstream layer that
//!   short-circuits with an error — rare but supported. Panics inside
//!   the handler do **not** fire this signal in axum (tower-http
//!   captures them at a different layer); the layer's job is signal
//!   plumbing, not panic catching.

use std::any::Any;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use tower::Service;

/// Future returned by request-signal receivers. `'static` because the
/// receiver is stored as `Arc<dyn ...>` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque identifier returned by `connect_*` for later use with
/// `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

// ---------------------------------------------------------------- Context types

/// Payload delivered to `request_started` receivers.
#[derive(Debug, Clone)]
pub struct RequestStartedContext {
    /// HTTP method (e.g. `"GET"`, `"POST"`).
    pub method: String,
    /// Request path, without the query string.
    pub path: String,
    /// Raw query string if present, empty otherwise.
    pub query: String,
}

/// Payload delivered to `request_finished` receivers.
#[derive(Debug, Clone)]
pub struct RequestFinishedContext {
    pub method: String,
    pub path: String,
    /// HTTP status code emitted by the handler.
    pub status: u16,
    /// Elapsed time from layer entry to response — milliseconds with
    /// fractional precision.
    pub elapsed_ms: f64,
}

/// Payload delivered to `got_request_exception` receivers — the inner
/// service returned an error rather than a `Response`.
#[derive(Debug, Clone)]
pub struct RequestExceptionContext {
    pub method: String,
    pub path: String,
    /// Stringified error from the inner service.
    pub error: String,
}

// ---------------------------------------------------------------- Internal storage

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SignalKind {
    Started,
    Finished,
    Exception,
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

/// Snapshot the receivers for `kind` into a `Vec<R>` so dispatch can
/// release the registry lock before awaiting any receiver future.
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

type StartedReceiver = Arc<dyn Fn(RequestStartedContext) -> ReceiverFuture + Send + Sync>;
type FinishedReceiver = Arc<dyn Fn(RequestFinishedContext) -> ReceiverFuture + Send + Sync>;
type ExceptionReceiver = Arc<dyn Fn(RequestExceptionContext) -> ReceiverFuture + Send + Sync>;

// ---------------------------------------------------------------- request_started

/// Register a `request_started` receiver. Fires before each request
/// reaches the inner service. Returns a [`ReceiverId`] for later
/// [`disconnect_request_started`].
pub fn connect_request_started<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(RequestStartedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: StartedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::Started, boxed)
}

/// Remove a previously-connected `request_started` receiver. Returns
/// `true` when an entry was removed.
pub fn disconnect_request_started(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::Started, id)
}

/// Fire `request_started` for `ctx`. Awaits every connected receiver
/// in registration order. Exposed for tests / custom dispatch — the
/// [`RequestSignalsLayer`] calls it for you in normal use.
pub async fn send_request_started(ctx: RequestStartedContext) {
    let receivers: Vec<StartedReceiver> = snapshot(SignalKind::Started);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- request_finished

/// Register a `request_finished` receiver. Fires after every response
/// the inner service returns (any status code).
pub fn connect_request_finished<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(RequestFinishedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: FinishedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::Finished, boxed)
}

/// Remove a previously-connected `request_finished` receiver.
pub fn disconnect_request_finished(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::Finished, id)
}

/// Fire `request_finished` for `ctx`.
pub async fn send_request_finished(ctx: RequestFinishedContext) {
    let receivers: Vec<FinishedReceiver> = snapshot(SignalKind::Finished);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- got_request_exception

/// Register a `got_request_exception` receiver. Fires when the inner
/// service returns an error rather than a response.
pub fn connect_got_request_exception<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(RequestExceptionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: ExceptionReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::Exception, boxed)
}

/// Remove a previously-connected `got_request_exception` receiver.
pub fn disconnect_got_request_exception(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::Exception, id)
}

/// Fire `got_request_exception` for `ctx`.
pub async fn send_got_request_exception(ctx: RequestExceptionContext) {
    let receivers: Vec<ExceptionReceiver> = snapshot(SignalKind::Exception);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Maintenance

/// Remove **all** request-signal receivers. Useful in tests to reset
/// registry state between cases.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Total receivers currently registered across all three request
/// signals. Useful in tests.
#[must_use]
pub fn receiver_count() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    [
        SignalKind::Started,
        SignalKind::Finished,
        SignalKind::Exception,
    ]
    .iter()
    .map(|k| reg.get(k).map_or(0, Vec::len))
    .sum()
}

// ---------------------------------------------------------------- Axum layer

/// Tower layer / axum middleware that fires `request_started` /
/// `request_finished` / `got_request_exception` around every request.
///
/// Mount it as the **outermost** layer of your `Router` so it sees the
/// request first and the response last — that way every other layer's
/// work counts toward `elapsed_ms`, and other layers' bodies aren't
/// surfaced as exceptions.
#[derive(Clone, Default, Debug)]
pub struct RequestSignalsLayer;

impl RequestSignalsLayer {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl<S> tower::Layer<S> for RequestSignalsLayer {
    type Service = RequestSignalsService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        RequestSignalsService { inner }
    }
}

/// The wrapped service produced by [`RequestSignalsLayer`].
#[derive(Clone)]
pub struct RequestSignalsService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for RequestSignalsService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // tower's call/clone pattern: clone the readied service into
        // the future, swap the local `inner` with the unreadied clone.
        // (Mirrors what axum::middleware::from_fn generates internally.)
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let method = req.method().as_str().to_owned();
        let path = req.uri().path().to_owned();
        let query = req.uri().query().unwrap_or_default().to_owned();

        Box::pin(async move {
            let started_at = Instant::now();
            send_request_started(RequestStartedContext {
                method: method.clone(),
                path: path.clone(),
                query,
            })
            .await;

            // S::Error is Infallible — `call` can't return Err. We still
            // pattern-match for future-proofing: if the bound is ever
            // widened, dispatching `got_request_exception` is wired up.
            match inner.call(req).await {
                Ok(resp) => {
                    let elapsed_ms = (started_at.elapsed().as_micros() as f64) / 1000.0;
                    let status = resp.status().as_u16();
                    send_request_finished(RequestFinishedContext {
                        method,
                        path,
                        status,
                        elapsed_ms,
                    })
                    .await;
                    Ok(resp)
                }
                Err(_unreachable) => {
                    // Infallible — this arm is dead code today but
                    // documents the intended dispatch. When S::Error
                    // widens (e.g. user wraps with a fallible layer),
                    // the exception receiver fires here.
                    #[allow(unreachable_code)]
                    {
                        send_got_request_exception(RequestExceptionContext {
                            method,
                            path,
                            error: "inner service returned Err".to_owned(),
                        })
                        .await;
                        Ok(error_response())
                    }
                }
            }
        })
    }
}

#[allow(dead_code)]
fn error_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("Internal Server Error"))
        .expect("static response builder")
}
