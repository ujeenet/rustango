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
//! ## What not to wrap
//!
//! Long-lived responses such as SSE streams and WebSocket upgrades.
//! Mount this on your API routes, not on the whole app.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;

/// How long a request may run.
#[derive(Clone, Debug)]
pub struct RequestTimeoutLayer {
    pub timeout: Duration,
}

impl RequestTimeoutLayer {
    /// Set the timeout. 30s suits most routes; give slower ones,
    /// such as uploads or batch jobs, more.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Read the timeout from settings. `None` when
    /// `request_timeout_secs` is unset or zero, so the timeout
    /// stays opt-in.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::ServerSettings) -> Option<Self> {
        let secs = s.request_timeout_secs?;
        if secs == 0 {
            return None;
        }
        Some(Self::new(Duration::from_secs(secs)))
    }
}

/// Adds `.request_timeout(layer)` to a router.
pub trait RequestTimeoutRouterExt {
    #[must_use]
    fn request_timeout(self, layer: RequestTimeoutLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RequestTimeoutRouterExt for Router<S> {
    fn request_timeout(self, layer: RequestTimeoutLayer) -> Self {
        let timeout = Arc::new(layer.timeout);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let timeout = timeout.clone();
                async move { handle(*timeout, req, next).await }
            },
        ))
    }
}

async fn handle(timeout: Duration, req: Request<Body>, next: Next) -> Response {
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

    use tower::ServiceExt as _;
}
