//! API versioning extractor — read the requested API version from URL,
//! header, or query parameter.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::api_version::{ApiVersion, VersionStrategy};
//!
//! // Inject the strategy as router extension
//! let app = Router::new()
//!     .route("/me", get(me))
//!     .layer(VersionStrategy::Header("X-API-Version").as_layer());
//!
//! // In your handler:
//! async fn me(version: ApiVersion) -> impl IntoResponse {
//!     match version.as_str() {
//!         "v1" => json!({"id": 1, "name": "Alice"}),
//!         "v2" => json!({"id": 1, "displayName": "Alice"}),
//!         _ => json!({"error": "unsupported version"}),
//!     }
//! }
//! ```

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::Router;

/// Strategy for extracting the API version from a request.
#[derive(Clone, Debug)]
pub enum VersionStrategy {
    /// Read the version from a request header (e.g. `"X-API-Version"`).
    Header(&'static str),
    /// Read the version from a query parameter (e.g. `"version"`).
    Query(&'static str),
    /// Read the version from the URL path's first segment (e.g. `/v1/users` → `"v1"`).
    UrlPrefix,
    /// Always use this version (useful for tests / fallback).
    Fixed(&'static str),
}

impl VersionStrategy {
    /// Extract the version from a request. Returns `None` if missing.
    pub fn extract(&self, parts: &Parts) -> Option<String> {
        match self {
            Self::Header(name) => parts
                .headers
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            Self::Query(name) => parts.uri.query().and_then(|q| {
                q.split('&').find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    if k == *name {
                        Some(v.to_owned())
                    } else {
                        None
                    }
                })
            }),
            Self::UrlPrefix => {
                let path = parts.uri.path();
                let first = path.trim_start_matches('/').split('/').next()?;
                if first.is_empty() {
                    None
                } else {
                    Some(first.to_owned())
                }
            }
            Self::Fixed(v) => Some((*v).to_owned()),
        }
    }

    /// Wrap as an axum middleware that injects [`ApiVersion`] into request extensions.
    #[must_use]
    pub fn as_layer<S: Clone + Send + Sync + 'static>(self) -> impl tower::Layer<Router<S>> + Clone {
        VersionLayerBuilder { strategy: Arc::new(self) }
    }
}

#[derive(Clone)]
struct VersionLayerBuilder {
    strategy: Arc<VersionStrategy>,
}

impl<S: Clone + Send + Sync + 'static> tower::Layer<Router<S>> for VersionLayerBuilder {
    type Service = Router<S>;

    fn layer(&self, inner: Router<S>) -> Self::Service {
        let strategy = self.strategy.clone();
        inner.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let strategy = strategy.clone();
                async move {
                    let (mut parts, body) = req.into_parts();
                    if let Some(v) = strategy.extract(&parts) {
                        parts.extensions.insert(ApiVersion(v));
                    }
                    let req = Request::from_parts(parts, body);
                    next.run(req).await
                }
            },
        ))
    }
}

/// Extracted API version. `None` if the strategy didn't find one and
/// no `Fixed` fallback was configured.
#[derive(Debug, Clone)]
pub struct ApiVersion(pub String);

impl ApiVersion {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ApiVersion {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ApiVersion>()
            .cloned()
            .unwrap_or_else(|| ApiVersion(String::new())))
    }
}

/// Helper: respond if the requested version isn't in the supported set.
/// Returns `Ok(version)` when supported, `Err(unsupported_version)` otherwise.
///
/// # Errors
/// Returns the rejected version string for the caller to surface in 400 / 406.
pub fn require_supported<'a>(
    requested: &str,
    supported: &'a [&'a str],
) -> Result<&'a str, String> {
    supported
        .iter()
        .find(|s| **s == requested)
        .copied()
        .ok_or_else(|| requested.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts_with_header(name: &str, value: &str) -> Parts {
        let req = Request::builder()
            .header(name, value)
            .uri("/anything")
            .body(())
            .unwrap();
        req.into_parts().0
    }

    fn parts_with_uri(uri: &str) -> Parts {
        Request::builder().uri(uri).body(()).unwrap().into_parts().0
    }

    #[test]
    fn header_strategy_reads_header() {
        let s = VersionStrategy::Header("X-API-Version");
        let p = parts_with_header("X-API-Version", "v2");
        assert_eq!(s.extract(&p).as_deref(), Some("v2"));
    }

    #[test]
    fn header_strategy_returns_none_when_missing() {
        let s = VersionStrategy::Header("X-API-Version");
        let p = parts_with_uri("/x");
        assert_eq!(s.extract(&p), None);
    }

    #[test]
    fn query_strategy_reads_param() {
        let s = VersionStrategy::Query("version");
        let p = parts_with_uri("/api/users?version=v3&other=foo");
        assert_eq!(s.extract(&p).as_deref(), Some("v3"));
    }

    #[test]
    fn url_prefix_strategy_reads_first_segment() {
        let s = VersionStrategy::UrlPrefix;
        let p = parts_with_uri("/v1/users/42");
        assert_eq!(s.extract(&p).as_deref(), Some("v1"));
    }

    #[test]
    fn url_prefix_strategy_with_root_returns_none() {
        let s = VersionStrategy::UrlPrefix;
        let p = parts_with_uri("/");
        assert_eq!(s.extract(&p), None);
    }

    #[test]
    fn fixed_strategy_always_returns_value() {
        let s = VersionStrategy::Fixed("v1");
        let p = parts_with_uri("/anything");
        assert_eq!(s.extract(&p).as_deref(), Some("v1"));
    }

    #[test]
    fn require_supported_accepts_known_version() {
        assert_eq!(require_supported("v2", &["v1", "v2", "v3"]), Ok("v2"));
    }

    #[test]
    fn require_supported_rejects_unknown_version() {
        let r = require_supported("v9", &["v1", "v2"]);
        assert_eq!(r, Err("v9".to_owned()));
    }
}
