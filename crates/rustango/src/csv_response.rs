//! A CSV download response. It sets `Content-Type` and, when you
//! give a filename, a `Content-Disposition` that makes the browser
//! offer a "Save as" dialog.
//!
//! Return it from any handler that produces CSV:
//!
//! ```ignore
//! use rustango::csv::csv_from_json_rows;
//! use rustango::csv_response::CsvResponse;
//! use axum::extract::State;
//!
//! async fn export_users(State(pool): State<PgPool>) -> CsvResponse {
//!     let rows = User::objects().fetch_all_as_json(&pool).await.unwrap();
//!     CsvResponse::new(csv_from_json_rows(&["id", "name", "email"], &rows))
//!         .filename("users.csv")
//! }
//! ```

use axum::body::Body;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};

use crate::csv::CsvWriter;

/// A CSV body plus an optional download filename.
pub struct CsvResponse {
    body: String,
    filename: Option<String>,
    /// `true` sends `Content-Disposition: attachment`, `false` sends
    /// `inline`. Defaults to `true`.
    attachment: bool,
}

impl CsvResponse {
    /// Build from a [`CsvWriter`]. Prefer this: the writer quotes
    /// fields and defuses spreadsheet formulas for you.
    #[must_use]
    pub fn new(writer: CsvWriter) -> Self {
        Self {
            body: writer.into_string(),
            filename: None,
            attachment: true,
        }
    }

    /// Build from a raw CSV string. Nothing is checked, so you must
    /// do the RFC 4180 quoting yourself, and you must defuse cells
    /// that start with `=`, `+`, `-` or `@`: a spreadsheet runs
    /// those as formulas. [`crate::csv::neutralize_formula`] does it.
    #[must_use]
    pub fn from_string(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            filename: None,
            attachment: true,
        }
    }

    /// Set the filename the browser suggests when saving.
    #[must_use]
    pub fn filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Show the CSV in the browser instead of downloading it.
    #[must_use]
    pub fn inline(mut self) -> Self {
        self.attachment = false;
        self
    }
}

impl IntoResponse for CsvResponse {
    fn into_response(self) -> Response {
        let mut resp = Response::new(Body::from(self.body));
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/csv; charset=utf-8"),
        );
        let disposition = match (&self.filename, self.attachment) {
            (Some(name), true) => format!("attachment; filename=\"{}\"", escape_filename(name)),
            (Some(name), false) => format!("inline; filename=\"{}\"", escape_filename(name)),
            (None, true) => "attachment".to_owned(),
            (None, false) => "inline".to_owned(),
        };
        if let Ok(v) = HeaderValue::from_str(&disposition) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        resp
    }
}

/// Escape `"` and `\` so the name is safe inside the quoted
/// `Content-Disposition` filename. Non-ASCII names would need the
/// RFC 5987 `filename*=` form, which this helper does not emit.
fn escape_filename(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::{csv_from_json_rows, CsvWriter};
    use axum::body::to_bytes;
    use serde_json::json;

    fn writer() -> CsvWriter {
        let mut w = CsvWriter::new();
        w.headers(&["id", "name"]);
        w.row(&["1", "Alice"]);
        w
    }

    #[tokio::test]
    async fn defaults_to_attachment_no_filename() {
        let resp = CsvResponse::new(writer()).into_response();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/csv; charset=utf-8"
        );
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_DISPOSITION)
                .unwrap()
                .to_str()
                .unwrap(),
            "attachment"
        );
        let bytes = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("id,name"));
        assert!(s.contains("1,Alice"));
    }

    #[tokio::test]
    async fn filename_lands_in_disposition_header() {
        let resp = CsvResponse::new(writer())
            .filename("users.csv")
            .into_response();
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_DISPOSITION)
                .unwrap()
                .to_str()
                .unwrap(),
            "attachment; filename=\"users.csv\""
        );
    }

    #[tokio::test]
    async fn inline_changes_disposition_type() {
        let resp = CsvResponse::new(writer()).inline().into_response();
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_DISPOSITION)
                .unwrap()
                .to_str()
                .unwrap(),
            "inline"
        );
    }

    #[tokio::test]
    async fn filename_with_quotes_is_escaped() {
        let resp = CsvResponse::new(writer())
            .filename(r#"oddly named "users".csv"#)
            .into_response();
        let v = resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(v, r#"attachment; filename="oddly named \"users\".csv""#);
    }

    #[tokio::test]
    async fn from_string_accepts_arbitrary_body() {
        let resp = CsvResponse::from_string("a,b\r\n1,2\r\n").into_response();
        let bytes = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "a,b\r\n1,2\r\n");
    }

    #[tokio::test]
    async fn round_trip_with_csv_from_json_rows() {
        let rows = vec![
            json!({"id": 1, "name": "Alice"}),
            json!({"id": 2, "name": "Bob, Jr."}),
        ];
        let resp = CsvResponse::new(csv_from_json_rows(&["id", "name"], &rows))
            .filename("users.csv")
            .into_response();
        let bytes = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // "Bob, Jr." is quoted because it holds a comma.
        assert!(s.starts_with("id,name\r\n"));
        assert!(s.contains("1,Alice"));
        assert!(s.contains("2,\"Bob, Jr.\""));
    }
}
