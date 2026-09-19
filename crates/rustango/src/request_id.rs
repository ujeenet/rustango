//! Request ID middleware — assign a unique ID to every incoming request.
//!
//! Adds an `X-Request-Id` response header and exposes the value via the
//! [`RequestId`] axum extractor.
//!
//! Honors an inbound `X-Request-Id` header by default (useful for chained
//! services that want to propagate IDs end-to-end), or always generates
//! a fresh one with [`RequestIdLayer::always_generate`].
//!
//! ## Getting the id onto your log lines
//!
//! **Mount [`crate::tracing_layer::TracingLayer`] as well**, and then
//! you do not have to do anything else. Since #1480 the request span
//! declares a `request_id` field and [`record`] fills it, so **every**
//! event emitted during the request carries it — including ones from
//! the ORM and from code that has never heard of a request id.
//!
//! That layer is not optional for this. [`record`] is
//! `Span::current().record("request_id", …)`, which is a **no-op**
//! when the current span has no such field — so this layer on its own
//! sets the response header and leaves your log lines bare.
//! `Cli::mount_observability` mounts both for you; wiring the router
//! by hand means mounting both by hand.
//!
//! This header used to show `tracing::info!(req_id = %id.0, …)` on
//! every call site instead. That is the tedious way, it silently
//! misses the events you did not write, and the field name differs
//! from the span's, so following it puts a *second*,
//! differently-named id on the same line (#1505). Correcting that
//! without naming the span layer was its own defect: a reader
//! dropping `req_id` from a router that mounts only this layer loses
//! correlation entirely, which is worse than what they started with
//! (#1606 review, correctness).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::request_id::{RequestIdLayer, RequestIdRouterExt, RequestId};
//! use rustango::tracing_layer::TracingLayer;
//!
//! let app = Router::new()
//!     .route("/me", get(handler))
//!     .request_id(RequestIdLayer::default())
//!     // Without this the id reaches the response header but not the log.
//!     .layer(TracingLayer::new());
//!
//! // No `req_id = …`: the span carries `request_id` for every event.
//! async fn handler(id: RequestId) -> String {
//!     tracing::info!("handling /me");
//!     format!("request {}", id.0)
//! }
//! ```

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request};
use axum::http::header::HeaderValue;
use axum::http::request::Parts;
use axum::http::Response;
use axum::middleware::Next;
use axum::Router;

const HEADER_NAME: &str = "x-request-id";

/// Configuration for the request-ID middleware.
#[derive(Clone)]
pub struct RequestIdLayer {
    /// When `true`, always generate a fresh ID even if the client sent one.
    /// Useful when you don't trust client-supplied values.
    pub always_generate: bool,
}

impl Default for RequestIdLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestIdLayer {
    /// Default: honor inbound `X-Request-Id`, generate one if absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            always_generate: false,
        }
    }

    /// Always generate a fresh ID — ignore any client-supplied value.
    #[must_use]
    pub fn always_generate() -> Self {
        Self {
            always_generate: true,
        }
    }
}

/// Extension trait — `.request_id(layer)` on Router.
pub trait RequestIdRouterExt {
    #[must_use]
    fn request_id(self, layer: RequestIdLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RequestIdRouterExt for Router<S> {
    fn request_id(self, layer: RequestIdLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

/// Extracted request ID. Always present when [`RequestIdLayer`] is in
/// the middleware stack — empty string otherwise.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl<S: Send + Sync> FromRequestParts<S> for RequestId {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<RequestId>()
            .cloned()
            .unwrap_or_else(|| RequestId(String::new())))
    }
}

async fn handle(cfg: Arc<RequestIdLayer>, mut req: Request<Body>, next: Next) -> Response<Body> {
    let id = if cfg.always_generate {
        generate_id()
    } else {
        req.headers()
            .get(HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty() && is_safe(s))
            .map_or_else(generate_id, str::to_owned)
    };

    // Put the id on the enclosing request span, so every event emitted
    // during this request carries it without the handler doing anything.
    //
    // This layer mounts inside `tracing_layer`'s span, so `current()` is
    // that span. Outside one — a hand-built router that mounts this and
    // not the span layer — `record` on a disabled span is a no-op, and
    // the extractor below still works.
    record(&id);

    req.extensions_mut().insert(RequestId(id.clone()));
    let mut response = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(HEADER_NAME, v);
    }
    response
}

/// Record `id` on the current request span.
///
/// Mirrors [`crate::tenant_log::record`]: the span declares
/// `request_id` as an empty field and this fills it partway through, so
/// the value reaches every event emitted under the span — the ORM's
/// included — without any of them knowing a request id exists.
///
/// Before #1480 the module's own documentation told you to write
/// `req_id = %id.0` on every call site by hand, which is both the
/// tedious way and the one that silently misses the events you did not
/// write.
pub fn record(id: &str) {
    tracing::Span::current().record("request_id", id);
}

/// Generate a 16-byte URL-safe random ID.
fn generate_id() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Reject inbound IDs with control chars, line breaks, or absurd lengths.
/// Defends against header-injection attacks via X-Request-Id.
fn is_safe(s: &str) -> bool {
    s.len() <= 128
        && s.chars()
            .all(|c| !c.is_control() && c != '\n' && c != '\r' && c != '\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_id_is_22_chars() {
        let id = generate_id();
        // 16 bytes base64-no-pad = ceil(16 * 4 / 3) = 22 chars
        assert_eq!(id.len(), 22);
    }

    #[test]
    fn generated_ids_are_unique() {
        let a = generate_id();
        let b = generate_id();
        assert_ne!(a, b);
    }

    #[test]
    fn is_safe_accepts_normal() {
        assert!(is_safe("abc-123_xyz"));
        assert!(is_safe("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn is_safe_rejects_long_strings() {
        let long = "x".repeat(129);
        assert!(!is_safe(&long));
    }

    #[test]
    fn is_safe_rejects_newlines() {
        assert!(!is_safe("abc\ndef"));
        assert!(!is_safe("abc\rdef"));
    }

    #[test]
    fn is_safe_rejects_null_bytes() {
        assert!(!is_safe("abc\0def"));
    }

    #[test]
    fn defaults_honor_inbound() {
        let l = RequestIdLayer::default();
        assert!(!l.always_generate);
    }

    #[test]
    fn always_generate_overrides() {
        let l = RequestIdLayer::always_generate();
        assert!(l.always_generate);
    }
}
