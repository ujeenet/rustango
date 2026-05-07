//! axum response wrapper for CSV exports — sets the right
//! `Content-Type` + an optional `Content-Disposition` so browsers
//! prompt a "Save as…" download.
//!
//! Use it as the return type of any handler that produces a CSV body:
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

/// CSV download response. Wraps a [`CsvWriter`] (or raw string) plus
/// optional download filename.
pub struct CsvResponse {
    body: String,
    filename: Option<String>,
    /// Whether to render `Content-Disposition: attachment` (force
    /// download) or `inline` (let the browser try to display).
    /// Default `true` (attachment).
    attachment: bool,
}

impl CsvResponse {
    /// New response from a [`CsvWriter`].
    #[must_use]
    pub fn new(writer: CsvWriter) -> Self {
        Self {
            body: writer.into_string(),
            filename: None,
            attachment: true,
        }
    }

    /// New response from a raw CSV string. Caller is responsible for
    /// formatting (RFC 4180 quoting, line terminators).
    #[must_use]
    pub fn from_string(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            filename: None,
            attachment: true,
        }
    }

    /// Set the download filename. The browser will use it for the
    /// "Save as…" dialog.
    #[must_use]
    pub fn filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Display inline (no download prompt). Default is attachment.
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

/// Escape `"` and `\` for inclusion inside the quoted filename of
/// `Content-Disposition`. Conservative — no Unicode encoding here;
/// for non-ASCII filenames you should use the RFC 5987 `filename*=`
/// extension (out of scope for the v1 helper).
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
        // Header row, then both data rows; "Bob, Jr." gets quoted because
        // it contains a comma.
        assert!(s.starts_with("id,name\r\n"));
        assert!(s.contains("1,Alice"));
        assert!(s.contains("2,\"Bob, Jr.\""));
    }
}
