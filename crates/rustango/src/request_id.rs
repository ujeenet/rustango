//! Request ID middleware: give every request a unique ID.
//!
//! It sets an `X-Request-Id` response header and exposes the value
//! through the [`RequestId`] extractor. By default an inbound
//! `X-Request-Id` is reused, so an ID can follow a call across
//! services. [`RequestIdLayer::always_generate`] ignores it.
//!
//! ## Getting the id into your logs
//!
//! Mount [`crate::tracing_layer::TracingLayer`] as well. Its request
//! span declares a `request_id` field and [`record`] fills it, so
//! every event during the request carries the id, including events
//! from the ORM. Without that layer [`record`] does nothing and you
//! get only the response header. `Cli::mount_observability` mounts
//! both for you.
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
//!
//! [`RequestId`]: crate::request_id::RequestId
//! [`RequestIdLayer::always_generate`]: crate::request_id::RequestIdLayer::always_generate
//! [`record`]: crate::request_id::record

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request};
use axum::http::header::HeaderValue;
use axum::http::request::Parts;
use axum::http::Response;
use axum::middleware::Next;
use axum::Router;

const HEADER_NAME: &str = "x-request-id";

/// Settings for the request-ID middleware.
#[derive(Clone)]
pub struct RequestIdLayer {
    /// Generate a fresh ID even when the client sent one. Use this
    /// when you do not trust client values.
    pub always_generate: bool,
}

impl Default for RequestIdLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestIdLayer {
    /// Reuse an inbound `X-Request-Id`, or generate one if absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            always_generate: false,
        }
    }

    /// Always generate a fresh ID and ignore what the client sent.
    #[must_use]
    pub fn always_generate() -> Self {
        Self {
            always_generate: true,
        }
    }
}

/// Adds `.request_id(layer)` to a router.
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

/// The request ID. Set when [`RequestIdLayer`] is mounted, and an
/// empty string when it is not.
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

    // Put the id on the enclosing request span so every event during
    // this request carries it. Without that span this is a no-op and
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
/// Like [`crate::tenant_log::record`]: the span declares an empty
/// `request_id` field and this fills it, so every event under the
/// span carries the id. It does nothing if no such span is active.
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

/// Reject inbound IDs that are too long or hold control characters
/// or line breaks. This blocks header injection via `X-Request-Id`.
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
