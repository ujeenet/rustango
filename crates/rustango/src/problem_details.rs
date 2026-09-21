//! RFC 7807 "Problem Details for HTTP APIs": error responses sent as
//! `application/problem+json`.
//!
//! [`crate::api_errors`] does the same job with rustango's own flat
//! `{error, message, status, details}` body. Use this module instead
//! when a public API or a gateway expects the RFC 7807 shape.
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
//! RFC 7807 allows extra fields next to the standard ones. Add them
//! with [`ProblemDetails::with_extension`]:
//!
//! ```ignore
//! ProblemDetails::validation("title cannot be empty")
//!     .with_extension("field", "title")
//!     .with_extension("rule", "non-empty")
//! ```
//!
//! ## Interop with `ApiError`
//!
//! With the `admin` feature on, `From<ApiError>` is available, so a
//! handler that returns `Result<T, ApiError>` can switch at the edge:
//!
//! ```ignore
//! handler().await.map_err(ProblemDetails::from)
//! ```
//!
//! [`ProblemDetails::with_extension`]: crate::problem_details::ProblemDetails::with_extension

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

/// One RFC 7807 problem document. It implements `IntoResponse`, so a
/// handler can return `Result<T, ProblemDetails>`.
#[derive(Debug, Clone)]
pub struct ProblemDetails {
    /// URI naming the kind of problem. Defaults to `"about:blank"`. A
    /// public API should point it at its error docs.
    pub type_: String,
    /// Short summary of the problem kind. It should stay the same for
    /// every occurrence. Defaults to the status reason phrase.
    pub title: String,
    /// HTTP status code, matching the response status line.
    pub status: u16,
    /// What went wrong this time, in plain words.
    pub detail: Option<String>,
    /// URI for this one occurrence, such as a request id or log link.
    pub instance: Option<String>,
    /// Extra fields. Order is stable, so output does not shift.
    pub extensions: IndexMap<String, Value>,
}

impl ProblemDetails {
    /// Build a problem from a status and a detail, with the default
    /// title.
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
    /// 422: the body parsed, but the values failed validation.
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

    /// Set the `type` URI, for example a link to your error docs.
    #[must_use]
    pub fn with_type(mut self, type_: impl Into<String>) -> Self {
        self.type_ = type_.into();
        self
    }

    /// Set the `title`. Only needed when you have a better summary than
    /// the status reason phrase.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set `detail`. Pass `None` to leave it out.
    #[must_use]
    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    /// Set the `instance` URI for this occurrence, often
    /// `"/errors/{request_id}"`.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Add an extra field. The reserved names `type`, `title`,
    /// `status`, `detail` and `instance` are ignored, because the spec
    /// does not allow overriding them this way.
    #[must_use]
    pub fn with_extension(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        let name = name.into();
        if !is_reserved(&name) {
            self.extensions.insert(name, value.into());
        }
        self
    }

    /// Render to a JSON [`Value`], for tests or a custom response.
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

// -------- bridge from ApiError

#[cfg(feature = "admin")]
impl From<crate::api_errors::ApiError> for ProblemDetails {
    fn from(e: crate::api_errors::ApiError) -> Self {
        let mut p = ProblemDetails::new(e.status, e.message);
        // Keep the ApiError slug ("validation_failed") visible by using it
        // as the title instead of the status reason phrase.
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
        // Compile-time check that the trait is implemented.
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
        // The slug becomes the title.
        assert_eq!(p.title, "validation_failed");
        assert_eq!(p.detail.as_deref(), Some("title cannot be empty"));
        assert_eq!(p.extensions["details"]["field"], "title");
    }
}
