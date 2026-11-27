//! Boot migrate's gate (6.11): the server binds before migrating, answers
//! `/health` 200, `/ready` 503 `migrating` and every other route 503 with
//! `Retry-After`, and opens on the real app once the run ends.

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower::ServiceExt as _;

/// Clones share one gate.
#[derive(Clone, Default)]
pub(crate) struct BootGate(Arc<OnceLock<Router>>);

impl BootGate {
    /// The router to serve from the first second.
    pub(crate) fn router(&self) -> Router {
        let gate = self.clone();
        Router::new().fallback(move |req: Request<Body>| {
            let gate = gate.clone();
            async move { gate.answer(req).await }
        })
    }

    async fn answer(&self, req: Request<Body>) -> Response {
        if let Some(app) = self.0.get() {
            return match app.clone().oneshot(req).await {
                Ok(r) => r,
                Err(never) => match never {},
            };
        }
        match req.uri().path() {
            "/health" => (StatusCode::OK, "ok").into_response(),
            "/ready" => (StatusCode::SERVICE_UNAVAILABLE, "migrating").into_response(),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "5")],
                "migrating",
            )
                .into_response(),
        }
    }

    /// Serve `app` from now on.
    pub(crate) fn open(&self, app: Router) {
        let _ = self.0.set(app);
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.0.get().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(app: &Router, path: &str) -> (StatusCode, Option<String>, String) {
        let res = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let retry = res
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, retry, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_gate_holds_routes_until_it_opens() {
        let gate = BootGate::default();
        let served = gate.router();
        assert_eq!(get(&served, "/health").await.0, StatusCode::OK);
        let ready = get(&served, "/ready").await;
        assert_eq!(
            (ready.0, ready.2.as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, "migrating")
        );
        let other = get(&served, "/api/x").await;
        assert_eq!(other.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(other.1.as_deref(), Some("5"));
        gate.open(Router::new().route("/api/x", axum::routing::get(|| async { "app" })));
        assert!(gate.is_open());
        let other = get(&served, "/api/x").await;
        assert_eq!((other.0, other.2.as_str()), (StatusCode::OK, "app"));
    }
}
