//! Request lifecycle signals, in Django's shape: `request_started`,
//! `request_finished` and `got_request_exception`.
//!
//! There is no model here, so receivers register globally. They run
//! one at a time, in registration order, around every request that
//! passes through [`RequestSignalsLayer`].
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
//! // Mount it outermost, so it sees the request first and the
//! // response last.
//! let app: Router = Router::new()
//!     // ... routes ...
//!     .layer(RequestSignalsLayer::new());
//! ```
//!
//! ## Rules
//!
//! - Receivers run one at a time, in registration order. For
//!   parallel work, or to keep a panic from stopping the rest of the
//!   chain, run the body in `tokio::spawn`.
//! - `request_started` runs before the inner service.
//! - `request_finished` runs after it returns, whatever the status.
//! - `got_request_exception` runs on a 5xx response, and on an error
//!   from the inner service. The second case is rare, since axum
//!   services are `Infallible`; it needs a layer that short-circuits
//!   with an error. A panic in a handler does **not** send this
//!   signal: axum catches those in another layer.

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

/// The future a receiver returns. It is `'static` because the
/// receiver is stored behind an `Arc` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Handle returned by `connect_*`, for a later `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

// ---------------------------------------------------------------- Context types

/// What a `request_started` receiver gets.
#[derive(Debug, Clone)]
pub struct RequestStartedContext {
    /// HTTP method, such as `"GET"` or `"POST"`.
    pub method: String,
    /// Request path, without the query string.
    pub path: String,
    /// Raw query string, or empty when there is none.
    pub query: String,
}

/// What a `request_finished` receiver gets.
#[derive(Debug, Clone)]
pub struct RequestFinishedContext {
    pub method: String,
    pub path: String,
    /// The status code the handler returned.
    pub status: u16,
    /// Milliseconds from entering the layer to the response, with a
    /// fractional part.
    pub elapsed_ms: f64,
}

/// What a `got_request_exception` receiver gets.
///
/// The layer sends this signal in two cases: a 5xx response, or an
/// `Err` from the inner service. The second is rare, since axum's
/// `Service::Error` is `Infallible`; it needs a layer that
/// short-circuits.
///
/// Use `status` to tell the two apart.
#[derive(Debug, Clone)]
pub struct RequestExceptionContext {
    pub method: String,
    pub path: String,
    /// The service error as text, or `"http <code>"` for a 5xx.
    pub error: String,
    /// `Some(code)` for a 5xx response, `None` for a service error.
    pub status: Option<u16>,
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

/// Copy the receivers for `kind` out of the registry, so the lock is
/// released before any of them is awaited.
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

/// Register a `request_started` receiver. It runs before the request
/// reaches the inner service. Returns an id for
/// [`disconnect_request_started`].
pub fn connect_request_started<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(RequestStartedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: StartedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::Started, boxed)
}

/// Remove a `request_started` receiver. `true` when one was removed.
pub fn disconnect_request_started(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::Started, id)
}

/// Send `request_started`, awaiting each receiver in registration
/// order. [`RequestSignalsLayer`] calls this for you; it is public
/// for tests and custom dispatch.
pub async fn send_request_started(ctx: RequestStartedContext) {
    let receivers: Vec<StartedReceiver> = snapshot(SignalKind::Started);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- request_finished

/// Register a `request_finished` receiver. It runs after every
/// response, whatever the status code.
pub fn connect_request_finished<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(RequestFinishedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: FinishedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::Finished, boxed)
}

/// Remove a `request_finished` receiver.
pub fn disconnect_request_finished(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::Finished, id)
}

/// Send `request_finished` for `ctx`.
pub async fn send_request_finished(ctx: RequestFinishedContext) {
    let receivers: Vec<FinishedReceiver> = snapshot(SignalKind::Finished);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- got_request_exception

/// Register a `got_request_exception` receiver. It runs on a 5xx
/// response, and when the inner service returns an error.
pub fn connect_got_request_exception<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(RequestExceptionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: ExceptionReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::Exception, boxed)
}

/// Remove a `got_request_exception` receiver.
pub fn disconnect_got_request_exception(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::Exception, id)
}

/// Send `got_request_exception` for `ctx`.
pub async fn send_got_request_exception(ctx: RequestExceptionContext) {
    let receivers: Vec<ExceptionReceiver> = snapshot(SignalKind::Exception);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Maintenance

/// Remove every request-signal receiver. Mostly for resetting state
/// between tests.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// How many receivers are registered across the three request
/// signals. Mostly useful in tests.
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

/// Tower layer that sends `request_started`, `request_finished` and
/// `got_request_exception` around every request.
///
/// Mount it as the **outermost** layer of your `Router`, so it sees
/// the request first and the response last. Then every other layer's
/// work counts toward `elapsed_ms`.
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
        // The usual tower pattern: move the readied service into the
        // future and keep the fresh clone here.
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

            // `S::Error` is `Infallible`, so `call` cannot return
            // `Err`. Match anyway: if the bound ever widens, the
            // exception signal is already wired.
            match inner.call(req).await {
                Ok(resp) => {
                    let elapsed_ms = (started_at.elapsed().as_micros() as f64) / 1000.0;
                    let status = resp.status().as_u16();
                    // Send the exception signal on a 5xx too. Axum's
                    // `Service::Error` is `Infallible` in practice,
                    // so without this branch the signal would never
                    // fire on a real failure.
                    if (500..600).contains(&status) {
                        send_got_request_exception(RequestExceptionContext {
                            method: method.clone(),
                            path: path.clone(),
                            error: format!("http {status}"),
                            status: Some(status),
                        })
                        .await;
                    }
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
                    // Unreachable while `S::Error` is `Infallible`.
                    // Kept so the signal already fires if a fallible
                    // layer widens that bound.
                    #[allow(unreachable_code)]
                    {
                        send_got_request_exception(RequestExceptionContext {
                            method,
                            path,
                            error: "inner service returned Err".to_owned(),
                            status: None,
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
