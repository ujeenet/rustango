//! Standardized API error responses.
//!
//! All errors share a consistent JSON shape so frontends can parse them
//! uniformly. Follows the RFC 7807 "Problem Details" convention loosely:
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
//! `ApiError` implements `axum::response::IntoResponse`, so any handler
//! returning `Result<T, ApiError>` produces a properly-shaped error response.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// Standardized API error.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl ApiError {
    /// Build an error with explicit status, code, and message.
    #[must_use]
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    /// Attach a structured `details` payload (e.g. field-error map).
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Convenience: attach `{"field": "..."}` details.
    #[must_use]
    pub fn with_field(self, field: impl Into<String>) -> Self {
        self.with_details(json!({"field": field.into()}))
    }

    // ---------------------------------------------------------------- presets

    /// `400 Bad Request` — `code = "bad_request"`.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// `401 Unauthorized` — `code = "unauthorized"`.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    /// `403 Forbidden` — `code = "forbidden"`.
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// `404 Not Found` — `code = "not_found"`.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// `409 Conflict` — `code = "conflict"`.
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// `422 Unprocessable Entity` — `code = "validation_failed"`.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            message,
        )
    }

    /// `429 Too Many Requests` — `code = "rate_limited"`.
    #[must_use]
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
    }

    /// `500 Internal Server Error` — `code = "internal_error"`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }

    /// `503 Service Unavailable` — `code = "service_unavailable"`.
    #[must_use]
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            message,
        )
    }

    /// Render to a JSON value (without going through `IntoResponse`).
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

    #[test]
    fn into_response_has_correct_status() {
        let e = ApiError::forbidden("nope");
        let r = e.into_response();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
