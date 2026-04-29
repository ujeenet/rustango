//! `AdminError` — request-handling failures with a uniform JSON body.
//!
//! Every view handler returns `Result<_, AdminError>`; the `IntoResponse`
//! impl turns each variant into the right HTTP status with a small JSON
//! payload describing what went wrong.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango_sql::sqlx;

use crate::forms::FormError;

#[derive(Debug)]
pub(crate) enum AdminError {
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

impl From<rustango_sql::ExecError> for AdminError {
    fn from(e: rustango_sql::ExecError) -> Self {
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
