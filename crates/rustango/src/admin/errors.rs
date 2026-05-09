//! `AdminError` — request-handling failures with a uniform JSON body.
//!
//! Every view handler returns `Result<_, AdminError>`; the `IntoResponse`
//! impl turns each variant into the right HTTP status with a small JSON
//! payload describing what went wrong.

use crate::sql::sqlx;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;

use super::forms::FormError;

/// Error returned by admin handlers — including user-defined bulk
/// action handlers registered via [`super::Builder::register_action`].
/// Variants are non-exhaustive only for `Internal`; user code should
/// almost always return [`AdminError::Internal`] from custom actions.
#[derive(Debug)]
pub enum AdminError {
    /// The table is in the model registry but not in the URL — i.e.
    /// the user hit a bad slug.
    TableNotFound {
        table: String,
    },
    /// The table IS registered but the underlying SQL table doesn't
    /// exist in the connected DB. Typically means migrations haven't
    /// been run yet (or haven't been run for the current tenant in a
    /// multi-tenant setup). Friendly HTML page nudges the user toward
    /// `cargo run -- migrate` / `migrate-tenants` instead of a raw
    /// Postgres error.
    TableMissing {
        table: String,
    },
    RowNotFound {
        table: String,
        pk: String,
    },
    ReadOnly {
        table: String,
    },
    Form(FormError),
    Internal(String),
}

/// PG SQLSTATE for "undefined table" — emitted when a SELECT/INSERT/
/// etc. references a relation that doesn't exist in the connected
/// schema. We use this to detect the "model registered but table not
/// yet migrated" case and surface a friendly hint.
const PG_UNDEFINED_TABLE: &str = "42P01";

fn pg_undefined_table_error(e: &sqlx::Error) -> Option<String> {
    let db = e.as_database_error()?;
    if db.code().as_deref() != Some(PG_UNDEFINED_TABLE) {
        return None;
    }
    // PG's UndefinedTable doesn't populate `.table()` reliably, but the
    // message reads `relation "<name>" does not exist`. Pull the
    // identifier out of the quotes when present.
    let msg = db.message();
    let name = msg
        .split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name.to_owned())
        .unwrap_or_else(|| "<unknown>".to_owned());
    Some(name)
}

impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
        if let Some(table) = pg_undefined_table_error(&e) {
            return Self::TableMissing { table };
        }
        Self::Internal(e.to_string())
    }
}

impl From<crate::sql::ExecError> for AdminError {
    fn from(e: crate::sql::ExecError) -> Self {
        // ExecError wraps sqlx errors; unwrap to detect PG_UNDEFINED_TABLE.
        if let crate::sql::ExecError::Driver(sqlx_err) = &e {
            if let Some(table) = pg_undefined_table_error(sqlx_err) {
                return Self::TableMissing { table };
            }
        }
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
            Self::TableMissing { table } => {
                let body = format!(
                    r##"<!doctype html>
<html><head><meta charset="utf-8"><title>Table not migrated — rustango admin</title>
<style>body{{font-family:system-ui,sans-serif;max-width:680px;margin:4em auto;padding:0 1em;color:#222}}
h1{{color:#b00;font-size:1.25em}} code{{background:#f3f3f3;padding:.1em .35em;border-radius:.2em}}
.hint{{background:#fff8dc;border-left:4px solid #d4a000;padding:.8em 1em;margin:1em 0}}</style>
</head><body>
<h1>Table <code>{table}</code> isn&rsquo;t in this database yet</h1>
<p>The model is registered but the underlying SQL table doesn&rsquo;t exist
in the connected schema. This usually means migrations haven&rsquo;t been
applied yet for this tenant / database.</p>
<div class="hint">
  <p><strong>Single-tenant?</strong> Run <code>cargo run -- migrate</code>.</p>
  <p><strong>Multi-tenant?</strong> Run <code>cargo run -- migrate-tenants</code>
  to apply tenant-scoped migrations to every active org.</p>
</div>
<p>Once migrations are applied this page will list the model&rsquo;s rows.</p>
</body></html>
"##,
                    table = html_escape(&table),
                );
                (StatusCode::SERVICE_UNAVAILABLE, Html(body)).into_response()
            }
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
            Self::Internal(msg) => {
                // v0.30.12 — log the raw message for the operator,
                // return a generic body to the client. Pre-v0.30.12
                // shape leaked DB error text (table names, column
                // names, sometimes SQL fragments) to anyone who
                // could trigger an internal error. The new shape
                // emits a request-correlatable id so operators can
                // grep their logs without exposing internals.
                let id = short_correlation_id();
                tracing::error!(
                    target: "rustango::admin",
                    correlation_id = %id,
                    error = %msg,
                    "admin internal error"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "internal",
                        "detail": "internal server error",
                        "correlation_id": id,
                    })),
                )
                    .into_response()
            }
        }
    }
}

/// Short correlation id stamped into both the log line and the
/// client response so operators can grep logs by the id the user
/// reports without ever exposing internal details to that user.
/// 8 bytes of `OsRng` rendered as 16 hex chars — collision-free
/// for any realistic error-rate window, short enough to read aloud.
fn short_correlation_id() -> String {
    use rand::{rngs::OsRng, RngCore};
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(16);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// `short_correlation_id` returns a 16-char hex string. Two
    /// calls return different ids (entropy check). 32 calls
    /// produce 32 distinct ids — collision-free at the rate any
    /// single admin instance would observe.
    #[test]
    fn short_correlation_id_shape_and_uniqueness() {
        for _ in 0..16 {
            let id = short_correlation_id();
            assert_eq!(id.len(), 16, "expected 16 hex chars, got `{id}`");
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit()),
                "expected hex only, got `{id}`"
            );
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..32 {
            seen.insert(short_correlation_id());
        }
        assert_eq!(seen.len(), 32, "expected all distinct correlation ids");
    }

    /// v0.30.12 (security audit) — AdminError::Internal must NOT
    /// echo the raw error text in the JSON body. Pre-fix the
    /// `detail` field carried the SQL / table / file path text
    /// straight to the client; post-fix it's a generic message
    /// and the raw text only goes to the operator's log.
    #[tokio::test]
    async fn internal_error_response_is_redacted() {
        let raw = "table \"sensitive_internal\" does not exist at SELECT * FROM sensitive_internal";
        let err = AdminError::Internal(raw.to_owned());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            !body_str.contains("sensitive_internal"),
            "raw error text leaked to client: {body_str}"
        );
        assert!(
            !body_str.contains("SELECT"),
            "SQL leaked to client: {body_str}"
        );
        assert!(
            body_str.contains("internal server error"),
            "expected generic message, got: {body_str}"
        );
        assert!(
            body_str.contains("correlation_id"),
            "expected correlation_id in body for log lookup, got: {body_str}"
        );
    }

    /// TableMissing path is unchanged — it's already a sanitized
    /// HTML page that mentions only the table name (which the
    /// user already typed in the URL, so it's not a leak).
    #[tokio::test]
    async fn table_missing_response_keeps_friendly_html() {
        let err = AdminError::TableMissing {
            table: "posts".to_owned(),
        };
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("posts"));
        assert!(body_str.contains("migrations"));
    }
}
