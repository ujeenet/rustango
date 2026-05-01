//! `AdminError` — request-handling failures with a uniform JSON body.
//!
//! Every view handler returns `Result<_, AdminError>`; the `IntoResponse`
//! impl turns each variant into the right HTTP status with a small JSON
//! payload describing what went wrong.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use crate::sql::sqlx;

use super::forms::FormError;

/// Error returned by admin handlers — including user-defined bulk
/// action handlers registered via [`super::Builder::register_action`].
/// Variants are non-exhaustive only for `Internal`; user code should
/// almost always return [`AdminError::Internal`] from custom actions.
#[derive(Debug)]
pub enum AdminError {
    TableNotFound { table: String },
    RowNotFound { table: String, pk: String },
    ReadOnly { table: String },
    Form(FormError),
    Internal(String),
}

impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<crate::sql::ExecError> for AdminError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::TableNotFound { table } => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "table not found", "table": table })),
            )
                .into_response(),
            Self::RowNotFound { table, pk } => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "row not found", "table": table, "pk": pk })),
            )
                .into_response(),
            Self::ReadOnly { table } => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "table is read-only", "table": table })),
            )
                .into_response(),
            Self::Form(e) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "form", "detail": e.to_string() })),
            )
                .into_response(),
            Self::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal", "detail": msg })),
            )
                .into_response(),
        }
    }
}
