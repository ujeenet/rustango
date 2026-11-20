//! One `atomic` block per write request, so a request that fails rolls back
//! every write it made on the request's pool.
//!
//! ```ignore
//! use rustango::atomic_request::AtomicRequests;
//! use rustango::sql::{AtomicOptions, Isolation};
//!
//! rustango::server::Builder::from_env().await?
//!     .api(routes)
//!     .atomic_requests(AtomicRequests::new().options(
//!         AtomicOptions::new().isolation(Isolation::ReadCommitted),
//!     ))
//! ```
//!
//! - POST, PUT, PATCH, DELETE and other unsafe methods run the handler in
//!   [`atomic`](crate::sql::atomic) on the request's pool. GET, HEAD,
//!   OPTIONS, TRACE and QUERY pass through with no `BEGIN`.
//! - A response that is not 2xx or 3xx rolls back and is sent as is. A
//!   panic rolls back and unwinds on to the outer panic guard.
//! - A failed `COMMIT` drops the handler's response, cookies included:
//!   a serialization failure, deadlock, lock timeout or `SQLITE_BUSY` is a
//!   `503` with `Retry-After`, anything else a `500`. Nothing is retried.
//! - SQLite blocks begin deferred, so a POST that writes nothing takes no
//!   write lock.
//!
//! Layers inside this one (those on your own router) run in the block;
//! what they write on the pool commits or rolls back with the request.
//! Writes that must survive a rollback go through
//! [`outside_atomic`](crate::sql::outside_atomic).

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse as _, Response};
use tower::{Layer, Service, ServiceExt as _};

use crate::sql::{AtomicOptions, ExecError, Pool};

/// What the server builders wrap the app's routes with. Its only knob is
/// the [`AtomicOptions`] each block begins with.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct AtomicRequests {
    opts: AtomicOptions,
}

impl AtomicRequests {
    /// Blocks begin as [`AtomicOptions::new`] says, but deferred on SQLite.
    pub fn new() -> Self {
        Self::default()
    }

    /// How each request's block begins.
    pub fn options(mut self, opts: AtomicOptions) -> Self {
        self.opts = opts;
        self
    }
}

#[derive(Clone)]
enum Source {
    Fixed(Pool),
    #[cfg(feature = "tenancy")]
    Tenant,
}

/// The layer behind [`AtomicRequests`], for a hand-built router. Put it
/// inside every layer that must see the final response after `COMMIT`
/// (idempotency stores, response caches) and outside the handler; on a
/// `Router` add it before those layers.
#[derive(Clone)]
#[must_use]
pub struct AtomicRequestLayer {
    source: Source,
    opts: AtomicOptions,
}

impl AtomicRequestLayer {
    /// Each write request runs in a block on `pool`.
    pub fn new(pool: Pool) -> Self {
        Self {
            source: Source::Fixed(pool),
            opts: AtomicOptions::new(),
        }
    }

    /// Each write request runs in a block on its tenant's pool, the one
    /// every tenant extractor in the request then serves. Requests with no
    /// tenant pass through; one whose tenant cannot be resolved gets `500`.
    #[cfg(feature = "tenancy")]
    pub fn tenant() -> Self {
        Self {
            source: Source::Tenant,
            opts: AtomicOptions::new(),
        }
    }

    /// How each request's block begins.
    pub fn options(mut self, opts: AtomicOptions) -> Self {
        self.opts = opts;
        self
    }

    /// This layer with `requests`' settings.
    pub(crate) fn with(self, requests: &AtomicRequests) -> Self {
        self.options(requests.opts.clone())
    }
}

impl<S> Layer<S> for AtomicRequestLayer {
    type Service = AtomicRequestService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AtomicRequestService {
            inner,
            source: self.source.clone(),
            opts: self.opts.clone(),
        }
    }
}

/// The service [`AtomicRequestLayer`] wraps a router in.
#[derive(Clone)]
pub struct AtomicRequestService<S> {
    inner: S,
    source: Source,
    opts: AtomicOptions,
}

impl<S> Service<Request<Body>> for AtomicRequestService<S>
where
    S: Service<Request<Body>, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        // Each call drives its own clone of `inner` to readiness.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let inner = self.inner.clone();
        let (source, opts) = (self.source.clone(), self.opts.clone());
        Box::pin(async move {
            if !writes(req.method()) {
                return inner.oneshot(req).await;
            }
            let (req, pool) = match source {
                Source::Fixed(pool) => (req, pool),
                #[cfg(feature = "tenancy")]
                Source::Tenant => match tenant_pool(req).await {
                    Choice::Pool(req, pool) => (req, pool),
                    Choice::Pass(req) => return inner.oneshot(req).await,
                    Choice::Refuse(resp) => return Ok(resp),
                },
            };
            Ok(run(&pool, opts, inner, req).await)
        })
    }
}

/// Whether `m` may write: everything but the safe methods.
fn writes(m: &Method) -> bool {
    !matches!(
        *m,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    ) && m.as_str() != "QUERY"
}

/// A 2xx or 3xx response commits; anything else rolls back.
fn keeps(status: StatusCode) -> bool {
    status.is_success() || status.is_redirection()
}

/// Run `inner` for `req` in one block on `pool`.
async fn run<S>(pool: &Pool, opts: AtomicOptions, inner: S, req: Request<Body>) -> Response
where
    S: Service<Request<Body>, Response = Response, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let request_id = request_id(&req);
    let out = crate::sql::atomic_request(
        pool,
        opts,
        move |_tx| {
            Box::pin(async move {
                let ran = crate::panic_guard::catch_unwind(inner.oneshot(req)).await;
                Ok(ran.map(|r| match r {
                    Ok(resp) => resp,
                    Err(never) => match never {},
                }))
            })
        },
        |ran: &std::thread::Result<Response>| ran.as_ref().is_ok_and(|r| keeps(r.status())),
    )
    .await;
    match out {
        Ok(Ok(resp)) => resp,
        // Rolled back; the outer panic guard logs it and answers 500.
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(e) => failed(&e, &request_id, &method, &path),
    }
}

fn request_id(req: &Request<Body>) -> String {
    #[cfg(feature = "_http_layers")]
    if let Some(id) = req.extensions().get::<crate::request_id::RequestId>() {
        return id.0.clone();
    }
    let _ = req;
    "-".to_owned()
}

/// The response for a block that could not begin or commit. The handler's
/// response describes writes that did not happen, so it is dropped.
fn failed(e: &ExecError, request_id: &str, method: &Method, path: &str) -> Response {
    if e.is_transient() {
        tracing::warn!(
            target: "rustango::atomic_request",
            request_id, method = %method, path, error = %e,
            "request transaction lost a conflict; answered 503"
        );
        let mut resp =
            crate::api_errors::ApiError::service_unavailable("conflict, try again").into_response();
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return resp;
    }
    tracing::error!(
        target: "rustango::error",
        request_id, method = %method, path, error = %e,
        "request transaction failed; its writes were rolled back"
    );
    let body = if crate::error::disclose_server_errors() {
        e.to_string()
    } else {
        crate::error::OPAQUE_SERVER_ERROR.to_owned()
    };
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(feature = "tenancy")]
enum Choice {
    Pool(Request<Body>, Pool),
    Pass(Request<Body>),
    Refuse(Response),
}

/// The tenant pool `req` writes to, stored on it so every extractor
/// further in serves the same one. Fails closed: a tenant whose pool is
/// unknown is refused, never served from the registry pool.
#[cfg(feature = "tenancy")]
async fn tenant_pool(req: Request<Body>) -> Choice {
    use crate::extractors::MountedTenantContext;
    use crate::tenancy::request_tenant::{store_org, DispatchedOrg, RequestPool};
    use crate::tenancy::RequestTenant;

    let refuse = |what: &str| {
        tracing::error!(target: "rustango::atomic_request", "{what}; refused the write");
        Choice::Refuse(crate::api_errors::ApiError::internal("tenant unavailable").into_response())
    };
    let (mut parts, body) = req.into_parts();
    match RequestTenant::get(&parts.extensions) {
        Some(RequestTenant::Global | RequestTenant::Unmatched) => {
            return Choice::Pass(Request::from_parts(parts, body));
        }
        Some(RequestTenant::Unknown) => return refuse("tenant resolution failed"),
        _ => {}
    }
    let Some(mounted) = MountedTenantContext::of(&parts.extensions) else {
        // No tenant context: no tenant pool for the handler to write to.
        return Choice::Pass(Request::from_parts(parts, body));
    };
    if parts.extensions.get::<DispatchedOrg>().is_none() {
        // A hand-built stack: resolve once here, for every layer further in.
        let org = match mounted.resolve(&parts).await {
            Ok(Some(org)) => org,
            Ok(None) => return Choice::Pass(Request::from_parts(parts, body)),
            Err(e) => return refuse(&format!("tenant resolution failed: {e}")),
        };
        store_org(&mut parts.extensions, &org);
    }
    let Some(org) = parts.extensions.get::<DispatchedOrg>().map(|d| d.0.clone()) else {
        return refuse("no tenant org");
    };
    if !RequestPool::stored(&parts.extensions) {
        return refuse(&format!("tenant `{}` has no stored pool", org.slug));
    }
    let Some(mounted) = MountedTenantContext::of(&parts.extensions) else {
        return refuse("tenant context gone");
    };
    match mounted.pool_for(&parts.extensions, &org).await {
        Ok(pool) => Choice::Pool(Request::from_parts(parts, body), pool),
        Err(e) => Choice::Refuse(crate::extractors::TenantRejection::from(e).into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mounted tenant context whose org has no stored pool (no id) is
    /// refused, never served from another pool.
    #[cfg(all(feature = "tenancy", feature = "sqlite"))]
    #[tokio::test]
    async fn a_tenant_without_a_stored_pool_is_refused() {
        use crate::extractors::TenantContext;
        use crate::tenancy::session::SessionSecret;
        use crate::tenancy::{ChainResolver, Org, OrgResolver, TenancyError, TenantPools};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct NoId;
        #[async_trait::async_trait]
        impl OrgResolver for NoId {
            async fn resolve(
                &self,
                _: &axum::http::request::Parts,
                _: &Pool,
            ) -> Result<Option<Org>, TenancyError> {
                Ok(Some(Org::new("noid")))
            }
        }
        let reg = crate::sql::sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        let ctx = Arc::new(TenantContext {
            pools: Arc::new(TenantPools::new(reg)),
            resolver: ChainResolver::new().push(NoId),
            session_secret: SessionSecret::from_bytes(vec![7u8; 32]),
            operator_secret: SessionSecret::from_bytes(vec![7u8; 32]),
        });
        let ran = Arc::new(AtomicBool::new(false));
        let seen = ran.clone();
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::post(move || {
                    seen.store(true, Ordering::SeqCst);
                    async { "ran" }
                }),
            )
            .layer(AtomicRequestLayer::tenant())
            .layer(axum::Extension(ctx));
        let req = Request::post("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !ran.load(Ordering::SeqCst),
            "the handler ran without a block"
        );
    }

    #[test]
    fn only_safe_methods_skip_the_block() {
        for m in ["GET", "HEAD", "OPTIONS", "TRACE", "QUERY"] {
            assert!(!writes(&Method::from_bytes(m.as_bytes()).unwrap()), "{m}");
        }
        for m in ["POST", "PUT", "PATCH", "DELETE", "PURGE"] {
            assert!(writes(&Method::from_bytes(m.as_bytes()).unwrap()), "{m}");
        }
    }

    #[test]
    fn only_2xx_and_3xx_commit() {
        assert!(keeps(StatusCode::CREATED));
        assert!(keeps(StatusCode::SEE_OTHER));
        for s in [400, 403, 404, 409, 500, 503] {
            assert!(!keeps(StatusCode::from_u16(s).unwrap()), "{s}");
        }
    }
}
