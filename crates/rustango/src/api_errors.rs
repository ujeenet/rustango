//! One JSON shape for every API error, so clients can parse them all
//! the same way. It follows RFC 7807 "Problem Details" loosely:
//!
//! ```json
//! {
//!   "error":   "validation_failed",
//!   "message": "title cannot be empty",
//!   "status":  422,
//!   "details": {"field": "title"}
//! }
//! ```
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::api_errors::ApiError;
//!
//! async fn create_post(body: Json<NewPost>) -> Result<Json<Post>, ApiError> {
//!     if body.title.is_empty() {
//!         return Err(ApiError::validation("title cannot be empty")
//!             .with_field("title"));
//!     }
//!     // ...
//! }
//! ```
//!
//! `ApiError` implements `IntoResponse`, so a handler returning
//! `Result<T, ApiError>` sends this shape on its own.
//!
//! The `message` and `details` go straight to the client. Keep
//! internal detail out of them: no SQL, stack traces, file paths or
//! secrets.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// An API error. Every field is sent to the client.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl ApiError {
    /// Build an error from a status, a code and a message.
    #[must_use]
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    /// Attach a `details` payload, such as a map of field errors.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Shorthand for details of `{"field": "..."}`.
    #[must_use]
    pub fn with_field(self, field: impl Into<String>) -> Self {
        self.with_details(json!({"field": field.into()}))
    }

    /// Build an error with the canonical code for `status`, so every
    /// framework surface names the same status the same way (#1193).
    #[must_use]
    pub fn from_status(status: StatusCode, message: impl Into<String>) -> Self {
        let code = match status {
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
            StatusCode::CONFLICT => "conflict",
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            StatusCode::UNPROCESSABLE_ENTITY => "validation_failed",
            StatusCode::TOO_MANY_REQUESTS => "rate_limited",
            StatusCode::BAD_GATEWAY => "bad_gateway",
            StatusCode::SERVICE_UNAVAILABLE => "service_unavailable",
            s if s.is_server_error() => "internal_error",
            _ => "error",
        };
        Self::new(status, code, message)
    }

    /// [`from_status`](Self::from_status), except a 5xx goes through
    /// `error::server_error_body`: logged under `context`, and sent only
    /// when `RUSTANGO_DISCLOSE_ERRORS` allows it.
    #[must_use]
    pub fn logged(status: StatusCode, context: &str, message: impl std::fmt::Display) -> Self {
        if status.is_server_error() {
            Self::from_status(status, crate::error::server_error_body(context, &message))
        } else {
            Self::from_status(status, message.to_string())
        }
    }

    // ---------------------------------------------------------------- presets

    /// `400 Bad Request` — `code = "bad_request"`.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::BAD_REQUEST, message)
    }

    /// `401 Unauthorized` — `code = "unauthorized"`.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::UNAUTHORIZED, message)
    }

    /// `403 Forbidden` — `code = "forbidden"`.
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::FORBIDDEN, message)
    }

    /// `404 Not Found` — `code = "not_found"`.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::NOT_FOUND, message)
    }

    /// `409 Conflict` — `code = "conflict"`.
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::CONFLICT, message)
    }

    /// `422 Unprocessable Entity` — `code = "validation_failed"`.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    /// `429 Too Many Requests` — `code = "rate_limited"`.
    #[must_use]
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::TOO_MANY_REQUESTS, message)
    }

    /// A finished `429` response with `Retry-After`, as every rate
    /// limiter sends it.
    #[must_use]
    pub fn rate_limited_response(message: impl Into<String>, retry_after_secs: u64) -> Response {
        let mut resp = Self::rate_limited(message)
            .with_details(json!({ "retry_after": retry_after_secs }))
            .into_response();
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, retry_after_secs.into());
        resp
    }

    /// `500 Internal Server Error` — `code = "internal_error"`.
    /// Pass a generic message; log the real cause instead of sending
    /// it to the client.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// `503 Service Unavailable` — `code = "service_unavailable"`.
    #[must_use]
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::from_status(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    /// Render the error as JSON, without building a response.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut body = json!({
            "error":   self.code,
            "message": self.message,
            "status":  self.status.as_u16(),
        });
        if let Some(details) = &self.details {
            body.as_object_mut()
                .unwrap()
                .insert("details".into(), details.clone());
        }
        body
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}: {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.to_json())).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_includes_required_fields() {
        let e = ApiError::not_found("post not found");
        let j = e.to_json();
        assert_eq!(j["error"], "not_found");
        assert_eq!(j["message"], "post not found");
        assert_eq!(j["status"], 404);
        assert!(j.get("details").is_none());
    }

    #[test]
    fn details_field_when_provided() {
        let e = ApiError::validation("invalid").with_field("email");
        let j = e.to_json();
        assert_eq!(j["details"]["field"], "email");
    }

    #[test]
    fn presets_carry_correct_status() {
        assert_eq!(ApiError::bad_request("x").status, StatusCode::BAD_REQUEST);
        assert_eq!(ApiError::unauthorized("x").status, StatusCode::UNAUTHORIZED);
        assert_eq!(ApiError::forbidden("x").status, StatusCode::FORBIDDEN);
        assert_eq!(ApiError::not_found("x").status, StatusCode::NOT_FOUND);
        assert_eq!(ApiError::conflict("x").status, StatusCode::CONFLICT);
        assert_eq!(
            ApiError::validation("x").status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ApiError::rate_limited("x").status,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ApiError::internal("x").status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::service_unavailable("x").status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn presets_use_canonical_codes() {
        assert_eq!(ApiError::bad_request("x").code, "bad_request");
        assert_eq!(ApiError::not_found("x").code, "not_found");
        assert_eq!(ApiError::validation("x").code, "validation_failed");
        assert_eq!(ApiError::rate_limited("x").code, "rate_limited");
        assert_eq!(ApiError::internal("x").code, "internal_error");
    }

    #[test]
    fn display_format_is_human_readable() {
        let e = ApiError::not_found("user 42 not found");
        let s = format!("{e}");
        assert!(s.contains("404"));
        assert!(s.contains("not_found"));
        assert!(s.contains("user 42 not found"));
    }

    #[test]
    fn with_details_overrides_field() {
        let e = ApiError::validation("invalid")
            .with_field("title") // sets details
            .with_details(json!({"custom": true})); // overrides
        assert!(e.details.unwrap().get("custom").is_some());
    }

    /// A driver message names the schema; a 5xx must not send it (#1193).
    #[test]
    fn logged_hides_a_server_errors_cause_but_not_a_client_errors() {
        let _env = crate::error::test_env::lock();
        crate::error::test_env::with("RUSTANGO_DISCLOSE_ERRORS", None, || {
            let e = ApiError::logged(
                StatusCode::INTERNAL_SERVER_ERROR,
                "test",
                "relation \"users\" missing",
            );
            assert_eq!(e.code, "internal_error");
            assert!(!e.message.contains("users"), "{e}");
        });

        let e = ApiError::logged(StatusCode::BAD_REQUEST, "test", "invalid cursor");
        assert_eq!(e.code, "bad_request");
        assert_eq!(e.message, "invalid cursor");
    }

    #[test]
    fn too_many_requests_sets_retry_after() {
        let r = ApiError::rate_limited_response("slow down", 7);
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()[axum::http::header::RETRY_AFTER], "7");
    }

    #[test]
    fn into_response_has_correct_status() {
        let e = ApiError::forbidden("nope");
        let r = e.into_response();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
