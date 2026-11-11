//! Cut off a request that runs too long. The handler is dropped and
//! the client gets `504 Gateway Timeout` instead of a hung
//! connection.
//!
//! ## When to use
//!
//! In production. One stuck database query or outbound HTTP call
//! can hold a worker forever, and enough of them starve every other
//! client. A timeout turns that into 504s you can see in metrics.
//!
//! `Cli::with_settings_from_env()` mounts this from
//! `Settings.server.request_timeout_secs`. Mount it yourself if you
//! build the server without `Cli`:
//!
//! ```ignore
//! use rustango::request_timeout::{RequestTimeoutLayer, RequestTimeoutRouterExt as _};
//! use std::time::Duration;
//!
//! let app = Router::new()
//!     .route("/api/posts", get(list_posts))
//!     .request_timeout(RequestTimeoutLayer::new(Duration::from_secs(30)));
//! ```
//!
//! ## Routes that run long
//!
//! The timeout covers the handler until it returns its response, so a
//! streamed body (SSE, a download) and a WebSocket after its upgrade are
//! not cut. A handler that reads a large upload is. Give such a route its
//! own timeout, or none, by the pattern it was registered with:
//!
//! ```
//! # use rustango::request_timeout::RequestTimeoutLayer;
//! # use std::time::Duration;
//! let layer = RequestTimeoutLayer::new(Duration::from_secs(30))
//!     .route("/api/exports", Duration::from_secs(300))
//!     .exempt("/api/uploads");
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;

/// How long a request may run.
#[derive(Clone, Debug)]
pub struct RequestTimeoutLayer {
    pub timeout: Duration,
    /// Timeouts that replace `timeout`, by route pattern; `None` is none.
    routes: HashMap<String, Option<Duration>>,
}

/// `timeout` when only route timeouts are configured: none.
const UNBOUNDED: Duration = Duration::MAX;

impl RequestTimeoutLayer {
    /// Set the timeout. 30s suits most routes; give slower ones,
    /// such as uploads or batch jobs, more.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            routes: HashMap::new(),
        }
    }

    /// Give the route registered as `pattern` (e.g. `"/files/{id}"`)
    /// `timeout` instead. The pattern must match the route as written,
    /// nest prefix included.
    #[must_use]
    pub fn route(mut self, pattern: impl Into<String>, timeout: Duration) -> Self {
        self.routes.insert(pattern.into(), Some(timeout));
        self
    }

    /// No timeout on the route registered as `pattern`.
    #[must_use]
    pub fn exempt(mut self, pattern: impl Into<String>) -> Self {
        self.routes.insert(pattern.into(), None);
        self
    }

    /// This request's timeout: its route's, else `timeout`.
    fn timeout_for(&self, req: &Request<Body>) -> Option<Duration> {
        let route = req.extensions().get::<MatchedPath>();
        let timeout = match route.and_then(|p| self.routes.get(p.as_str())) {
            Some(t) => *t,
            None => Some(self.timeout),
        };
        timeout.filter(|t| *t != UNBOUNDED)
    }

    /// Read the timeout and its `[server.routes]` overrides from
    /// settings; a route's `0` exempts it. `None` when no timeout is set
    /// (unset or zero), so the timeout stays opt-in.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::ServerSettings) -> Option<Self> {
        let global = s.request_timeout_secs.filter(|n| *n > 0);
        let mut layer = Self::new(global.map_or(UNBOUNDED, Duration::from_secs));
        for (pattern, r) in &s.routes {
            layer = match r.request_timeout_secs {
                Some(0) => layer.exempt(pattern.clone()),
                Some(n) => layer.route(pattern.clone(), Duration::from_secs(n)),
                None => layer,
            };
        }
        let bounded = layer.routes.values().any(Option::is_some);
        (global.is_some() || bounded).then_some(layer)
    }
}

/// Adds `.request_timeout(layer)` to a router.
pub trait RequestTimeoutRouterExt {
    #[must_use]
    fn request_timeout(self, layer: RequestTimeoutLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RequestTimeoutRouterExt for Router<S> {
    fn request_timeout(self, layer: RequestTimeoutLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(&cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: &RequestTimeoutLayer, req: Request<Body>, next: Next) -> Response {
    let Some(timeout) = cfg.timeout_for(&req) else {
        return next.run(req).await;
    };
    match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => {
            tracing::warn!(
                target: "rustango::request_timeout",
                timeout_secs = timeout.as_secs(),
                "request handler exceeded timeout — returning 504",
            );
            (
                StatusCode::GATEWAY_TIMEOUT,
                "request handler exceeded the configured timeout",
            )
                .into_response()
        }
    }
}

use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::routing::get;

    #[test]
    fn new_stores_timeout() {
        let l = RequestTimeoutLayer::new(Duration::from_secs(30));
        assert_eq!(l.timeout.as_secs(), 30);
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_unset_returns_none() {
        let s = crate::config::ServerSettings::default();
        assert!(RequestTimeoutLayer::from_settings(&s).is_none());
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_zero_returns_none() {
        let mut s = crate::config::ServerSettings::default();
        s.request_timeout_secs = Some(0);
        assert!(RequestTimeoutLayer::from_settings(&s).is_none());
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_picks_up_seconds() {
        let mut s = crate::config::ServerSettings::default();
        s.request_timeout_secs = Some(30);
        let l = RequestTimeoutLayer::from_settings(&s).expect("Some");
        assert_eq!(l.timeout.as_secs(), 30);
    }

    #[tokio::test]
    async fn fast_handler_passes_through() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .request_timeout(RequestTimeoutLayer::new(Duration::from_secs(5)));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn slow_handler_504s() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    "should never reach"
                }),
            )
            .request_timeout(RequestTimeoutLayer::new(Duration::from_millis(10)));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    fn sleeper() -> axum::routing::MethodRouter {
        get(|| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            "done"
        })
    }

    async fn status(app: &Router, uri: &str) -> StatusCode {
        let req = axum::http::Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    /// An exempt route runs past the timeout; a route's own timeout
    /// replaces it; other routes keep it.
    #[tokio::test]
    async fn route_timeouts_replace_the_global_one() {
        let app = Router::new()
            .route("/upload", sleeper())
            .route("/export/{id}", sleeper())
            .route("/", sleeper())
            .request_timeout(
                RequestTimeoutLayer::new(Duration::from_millis(10))
                    .exempt("/upload")
                    .route("/export/{id}", Duration::from_secs(5)),
            );
        assert_eq!(status(&app, "/upload").await, StatusCode::OK);
        assert_eq!(status(&app, "/export/1").await, StatusCode::OK);
        assert_eq!(status(&app, "/").await, StatusCode::GATEWAY_TIMEOUT);
    }

    /// The timeout ends at the response head: a streamed body, as SSE
    /// sends, runs on past it.
    #[tokio::test]
    async fn a_streamed_body_outlives_the_timeout() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let late = futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Ok::<_, std::convert::Infallible>("late")
                    });
                    Body::from_stream(late)
                }),
            )
            .request_timeout(RequestTimeoutLayer::new(Duration::from_millis(10)));
        let req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(&to_bytes(resp.into_body(), 64).await.unwrap()[..], b"late");
    }

    /// `[server.routes]` timeouts: `0` exempts; one alone installs the
    /// layer with no timeout elsewhere.
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn from_settings_takes_route_timeouts() {
        let limits = |secs| {
            let mut r = crate::config::RouteLimits::default();
            r.request_timeout_secs = Some(secs);
            r
        };
        let mut s = crate::config::ServerSettings::default();
        s.routes.insert("/upload".into(), limits(0));
        assert!(
            RequestTimeoutLayer::from_settings(&s).is_none(),
            "nothing to time"
        );
        s.request_timeout_secs = Some(30);
        let l = RequestTimeoutLayer::from_settings(&s).expect("Some");
        assert_eq!(l.routes.get("/upload"), Some(&None));

        let mut s = crate::config::ServerSettings::default();
        s.routes.insert("/slow".into(), limits(1));
        let app = Router::new()
            .route("/", sleeper())
            .route("/slow", sleeper())
            .request_timeout(RequestTimeoutLayer::from_settings(&s).expect("Some"));
        assert_eq!(status(&app, "/").await, StatusCode::OK, "no global timeout");
        assert_eq!(status(&app, "/slow").await, StatusCode::OK);
    }

    use tower::ServiceExt as _;
}
