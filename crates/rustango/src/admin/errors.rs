//! `AdminError`: request-handling failures with a uniform JSON body.
//!
//! Every view handler returns `Result<_, AdminError>`. The
//! `IntoResponse` impl maps each variant to an HTTP status and a small
//! JSON payload.

use crate::sql::sqlx;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;

use super::forms::FormError;

/// Error returned by admin handlers, including bulk actions registered
/// with [`super::Builder::register_action`]. Custom actions should
/// almost always return [`AdminError::Internal`].
#[derive(Debug)]
pub enum AdminError {
    /// No such table in the model registry: the URL slug is wrong.
    TableNotFound {
        table: String,
    },
    /// The model is registered but its SQL table is not in the
    /// connected database, so migrations have probably not run for this
    /// tenant yet. Renders a page pointing at `migrate` and
    /// `migrate-tenants` instead of a raw driver error.
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
    /// A per-object permission hook, registered with
    /// `register_admin_object_permission!`, said no for this row.
    /// Renders as a 403.
    Forbidden {
        table: String,
        action: &'static str,
    },
    Form(FormError),
    Internal(String),
}

/// Postgres SQLSTATE for "undefined table". Used to spot the "model
/// registered but not yet migrated" case and show a hint.
const PG_UNDEFINED_TABLE: &str = "42P01";

/// MySQL `ER_NO_SUCH_TABLE`.
const MYSQL_NO_SUCH_TABLE: &str = "1146";

fn pg_undefined_table_error(e: &sqlx::Error) -> Option<String> {
    let db = e.as_database_error()?;
    if db.code().as_deref() != Some(PG_UNDEFINED_TABLE) {
        return None;
    }
    // `.table()` is not reliable here, but the message reads
    // `relation "<name>" does not exist`. Take the quoted identifier.
    let msg = db.message();
    let name = msg
        .split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name.to_owned())
        .unwrap_or_else(|| "<unknown>".to_owned());
    Some(name)
}

/// SQLite has no structured code for a missing table, only the message
/// `no such table: <name>`. Match that shape so SQLite hosts get the
/// same `TableMissing` page as Postgres hosts, not a 500.
fn sqlite_undefined_table_error(e: &sqlx::Error) -> Option<String> {
    let db = e.as_database_error()?;
    let msg = db.message();
    let rest = msg.strip_prefix("no such table: ")?;
    // Trailing context may follow the name: take the first identifier.
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// MySQL reports a missing table as code 1146 with a message like
/// `Table 'db.table' doesn't exist`. Match the code first, then take
/// the unqualified name from the message.
fn mysql_undefined_table_error(e: &sqlx::Error) -> Option<String> {
    let db = e.as_database_error()?;
    if db.code().as_deref() != Some(MYSQL_NO_SUCH_TABLE) {
        return None;
    }
    // Message shape: `Table 'demo.rustango_admin_users' doesn't exist`.
    let msg = db.message();
    let (_, rest) = msg.split_once('\'')?;
    let (qualified, _) = rest.split_once('\'')?;
    let name = qualified
        .rsplit_once('.')
        .map(|(_, t)| t)
        .unwrap_or(qualified);
    Some(name.to_owned())
}

fn undefined_table_error(e: &sqlx::Error) -> Option<String> {
    pg_undefined_table_error(e)
        .or_else(|| sqlite_undefined_table_error(e))
        .or_else(|| mysql_undefined_table_error(e))
}

impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
        if let Some(table) = undefined_table_error(&e) {
            return Self::TableMissing { table };
        }
        Self::Internal(e.to_string())
    }
}

impl From<crate::sql::ExecError> for AdminError {
    fn from(e: crate::sql::ExecError) -> Self {
        // Unwrap the sqlx error so the per-dialect undefined-table
        // check still runs and hosts get `TableMissing`, not a 500.
        if let crate::sql::ExecError::Driver(sqlx_err) = &e {
            if let Some(table) = undefined_table_error(sqlx_err) {
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
            Self::Forbidden { table, action } => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "permission denied",
                    "table": table,
                    "action": action,
                })),
            )
                .into_response(),
            Self::Form(e) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "form", "detail": e.to_string() })),
            )
                .into_response(),
            Self::Internal(msg) => {
                // Log the raw message for the operator and return a
                // generic body. The raw text can hold table names,
                // column names and SQL, so it must not reach the
                // client. The response carries only an id the operator
                // can grep for in the logs.
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

/// Short id put in both the log line and the client response, so an
/// operator can find the log entry from what the user reports without
/// any internal detail reaching that user. 8 bytes of `OsRng` as 16 hex
/// chars: short enough to read aloud.
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

    /// `short_correlation_id` returns 16 hex chars, and 32 calls give
    /// 32 distinct ids.
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

    /// `AdminError::Internal` must **not** echo the raw error text in
    /// the JSON body: SQL, table names and file paths stay in the
    /// operator's log only.
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

    // The SQLite and MySQL parsers read the table name out of each
    // driver's message, so `TableMissing` fires on every backend.
    #[test]
    fn sqlite_undefined_table_extracts_name_from_message() {
        let raw = "(code: 1) no such table: rustango_admin_users";
        // sqlx may or may not include the libsqlite3 prefix. What
        // matters is the `no such table: …` part of `.message()`.
        let msg = raw.strip_prefix("(code: 1) ").unwrap_or(raw);
        assert_eq!(
            msg.strip_prefix("no such table: ").map(|rest| {
                rest.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            }),
            Some("rustango_admin_users".to_owned())
        );
    }

    #[test]
    fn mysql_undefined_table_extracts_name_from_message() {
        // MySQL's `.message()` reads `Table 'db.table' doesn't exist`.
        let msg = "Table 'demo.rustango_admin_users' doesn't exist";
        let (_, rest) = msg.split_once('\'').unwrap();
        let (qualified, _) = rest.split_once('\'').unwrap();
        let name = qualified
            .rsplit_once('.')
            .map(|(_, t)| t)
            .unwrap_or(qualified);
        assert_eq!(name, "rustango_admin_users");
    }

    /// The `TableMissing` page names only the table, which the user
    /// already typed in the URL, so it leaks nothing.
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
