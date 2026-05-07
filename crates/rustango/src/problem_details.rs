//! RFC 7807 "Problem Details for HTTP APIs" — standardized error
//! responses with the canonical `application/problem+json` content
//! type.
//!
//! Sister module to [`crate::api_errors`]: ApiError ships rustango's
//! flat `{error, message, status, details}` shape that frontends
//! already parse. ProblemDetails ships the RFC 7807 shape that public
//! REST APIs (Stripe, GitHub, Twitter all loose variants of it) and
//! API gateways expect.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::problem_details::ProblemDetails;
//!
//! async fn fetch_post(id: i64) -> Result<Json<Post>, ProblemDetails> {
//!     load(id).await.ok_or_else(|| ProblemDetails::not_found(
//!         format!("no post with id={id}"),
//!     ))
//! }
//! ```
//!
//! ```json
//! HTTP/1.1 404 Not Found
//! Content-Type: application/problem+json
//!
//! {
//!   "type":   "about:blank",
//!   "title":  "Not Found",
//!   "status": 404,
//!   "detail": "no post with id=42"
//! }
//! ```
//!
//! ## Extension fields
//!
//! RFC 7807 explicitly allows arbitrary extra fields alongside the
//! standard ones. Add them with [`ProblemDetails::with_extension`]:
//!
//! ```ignore
//! ProblemDetails::validation("title cannot be empty")
//!     .with_extension("field", "title")
//!     .with_extension("rule", "non-empty")
//! ```
//!
//! ## Interop with `ApiError`
//!
//! `ProblemDetails: From<ApiError>` is implemented when both modules
//! are on, so handlers that currently return `Result<T, ApiError>` can
//! emit RFC 7807 by mapping at the boundary:
//!
//! ```ignore
//! handler().await.map_err(ProblemDetails::from)
//! ```

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

/// One RFC 7807 problem document. Implements `axum::response::IntoResponse`
/// so handlers can return `Result<T, ProblemDetails>` directly.
#[derive(Debug, Clone)]
pub struct ProblemDetails {
    /// URI reference identifying the problem type. Defaults to
    /// `"about:blank"` per the spec — pointing at human-readable docs
    /// is a best practice for public APIs.
    pub type_: String,
    /// Short human-readable summary. SHOULD NOT change between
    /// occurrences of the same problem (per spec). Defaults to the
    /// HTTP status canonical reason phrase.
    pub title: String,
    /// HTTP status code. Same value as the response's status line.
    pub status: u16,
    /// Human-readable explanation specific to this occurrence.
    pub detail: Option<String>,
    /// URI reference identifying THIS specific occurrence (typically a
    /// per-request UUID or a log-search URL).
    pub instance: Option<String>,
    /// Extension members — arbitrary additional fields. Stable order
    /// (IndexMap) so successive serializations match.
    pub extensions: IndexMap<String, Value>,
}

impl ProblemDetails {
    /// Build a problem with `status` + the default title + `detail`.
    #[must_use]
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            type_: "about:blank".into(),
            title: status.canonical_reason().unwrap_or("Unknown").to_owned(),
            status: status.as_u16(),
            detail: Some(detail.into()),
            instance: None,
            extensions: IndexMap::new(),
        }
    }

    // -------- common shortcuts

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, detail)
    }
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail)
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, detail)
    }
    /// 422 — semantically "the body is well-formed but failed validation".
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }
    pub fn too_many_requests(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, detail)
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, detail)
    }

    // -------- builder

    /// Override the `type` URI. For public APIs, point at human-readable
    /// docs (e.g. `"https://docs.example.com/errors/validation"`).
    #[must_use]
    pub fn with_type(mut self, type_: impl Into<String>) -> Self {
        self.type_ = type_.into();
        self
    }

    /// Override the `title`. Default is the status canonical reason —
    /// only set this if you have a more specific summary.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Override `detail`. Pass `None` to drop it (per spec, detail is
    /// optional).
    #[must_use]
    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    /// Set the `instance` URI — the canonical identifier for THIS
    /// occurrence. Common pattern: `"/errors/{request_id}"`.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Add an extension field. RFC 7807 explicitly permits arbitrary
    /// extra fields next to the standard ones. Reserved names
    /// (`type`, `title`, `status`, `detail`, `instance`) are silently
    /// ignored — the spec says they shouldn't be overridden via
    /// extension.
    #[must_use]
    pub fn with_extension(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        let name = name.into();
        if !is_reserved(&name) {
            self.extensions.insert(name, value.into());
        }
        self
    }

    /// Render to a JSON [`Value`]. Useful for tests and for embedding
    /// in custom response builders.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

fn is_reserved(name: &str) -> bool {
    matches!(name, "type" | "title" | "status" | "detail" | "instance")
}

impl Serialize for ProblemDetails {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let extra = usize::from(self.detail.is_some()) + usize::from(self.instance.is_some());
        let mut m = serializer.serialize_map(Some(3 + extra + self.extensions.len()))?;
        m.serialize_entry("type", &self.type_)?;
        m.serialize_entry("title", &self.title)?;
        m.serialize_entry("status", &self.status)?;
        if let Some(detail) = &self.detail {
            m.serialize_entry("detail", detail)?;
        }
        if let Some(instance) = &self.instance {
            m.serialize_entry("instance", instance)?;
        }
        for (k, v) in &self.extensions {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl IntoResponse for ProblemDetails {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| b"{}".to_vec());
        let mut resp = Response::new(axum::body::Body::from(body));
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        resp
    }
}

impl std::fmt::Display for ProblemDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}{}",
            self.status,
            self.title,
            self.detail
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        )
    }
}

impl std::error::Error for ProblemDetails {}

// -------- bridge to ApiError so existing handlers can opt in by mapping

#[cfg(feature = "admin")]
impl From<crate::api_errors::ApiError> for ProblemDetails {
    fn from(e: crate::api_errors::ApiError) -> Self {
        let mut p = ProblemDetails::new(e.status, e.message);
        // The ApiError carries a `code` slug like "validation_failed"
        // — promote it to the title so the RFC 7807 doc keeps the slug
        // in plain sight (default title is the status canonical reason).
        if e.code != p.title.to_lowercase().replace(' ', "_") {
            p = p.with_title(e.code);
        }
        if let Some(details) = e.details {
            p = p.with_extension("details", details);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn new_default_title_is_status_phrase() {
        let p = ProblemDetails::not_found("missing");
        assert_eq!(p.title, "Not Found");
        assert_eq!(p.status, 404);
        assert_eq!(p.type_, "about:blank");
        assert_eq!(p.detail.as_deref(), Some("missing"));
    }

    #[test]
    fn shortcuts_pick_correct_status_codes() {
        assert_eq!(ProblemDetails::bad_request("x").status, 400);
        assert_eq!(ProblemDetails::unauthorized("x").status, 401);
        assert_eq!(ProblemDetails::forbidden("x").status, 403);
        assert_eq!(ProblemDetails::not_found("x").status, 404);
        assert_eq!(ProblemDetails::conflict("x").status, 409);
        assert_eq!(ProblemDetails::validation("x").status, 422);
        assert_eq!(ProblemDetails::too_many_requests("x").status, 429);
        assert_eq!(ProblemDetails::internal("x").status, 500);
    }

    #[test]
    fn serializes_with_standard_field_order() {
        let p = ProblemDetails::not_found("nope")
            .with_instance("/errors/req-1")
            .with_extension("trace_id", "abc-123");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(v["type"], "about:blank");
        assert_eq!(v["title"], "Not Found");
        assert_eq!(v["status"], 404);
        assert_eq!(v["detail"], "nope");
        assert_eq!(v["instance"], "/errors/req-1");
        assert_eq!(v["trace_id"], "abc-123");
    }

    #[test]
    fn extension_with_reserved_name_is_ignored() {
        let p = ProblemDetails::bad_request("x").with_extension("status", 999);
        assert_eq!(p.status, 400, "reserved field is not overridden");
        let v = p.to_value();
        assert_eq!(v["status"], 400);
    }

    #[test]
    fn detail_omitted_when_set_to_none() {
        let p = ProblemDetails::not_found("nope").with_detail(None);
        let v = p.to_value();
        assert!(v.get("detail").is_none());
    }

    #[test]
    fn instance_omitted_by_default() {
        let p = ProblemDetails::not_found("nope");
        let v = p.to_value();
        assert!(v.get("instance").is_none());
    }

    #[tokio::test]
    async fn into_response_uses_problem_json_content_type() {
        let resp = ProblemDetails::not_found("missing").into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/problem+json"
        );
        let bytes = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], 404);
        assert_eq!(v["detail"], "missing");
    }

    #[tokio::test]
    async fn into_response_invalid_status_falls_back_to_500() {
        let mut p = ProblemDetails::not_found("nope");
        p.status = 9999; // invalid
        let resp = p.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn display_includes_status_title_detail() {
        let p = ProblemDetails::validation("title cannot be empty");
        assert_eq!(
            p.to_string(),
            "422 Unprocessable Entity: title cannot be empty"
        );
    }

    #[test]
    fn display_omits_colon_when_no_detail() {
        let p = ProblemDetails::not_found("x").with_detail(None);
        assert_eq!(p.to_string(), "404 Not Found");
    }

    #[test]
    fn implements_std_error() {
        // Compile-time check — ProblemDetails: std::error::Error.
        fn assert_error<E: std::error::Error>() {}
        assert_error::<ProblemDetails>();
    }

    #[cfg(feature = "admin")]
    #[test]
    fn from_api_error_promotes_slug_to_title() {
        use crate::api_errors::ApiError;
        let e = ApiError::validation("title cannot be empty").with_field("title");
        let p: ProblemDetails = e.into();
        assert_eq!(p.status, 422);
        // The ApiError's `error` slug ("validation_failed") becomes the title.
        assert_eq!(p.title, "validation_failed");
        assert_eq!(p.detail.as_deref(), Some("title cannot be empty"));
        assert_eq!(p.extensions["details"]["field"], "title");
    }
}
