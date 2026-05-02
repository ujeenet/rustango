//! Minimal CSV writer — RFC 4180 compliant, zero deps.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::csv::CsvWriter;
//!
//! let mut w = CsvWriter::new();
//! w.headers(&["id", "name", "email"]);
//! w.row(&["1", "Alice", "alice@example.com"]);
//! w.row(&["2", "Bob", "bob, jr.@example.com"]);   // commas auto-quoted
//! let csv = w.into_string();
//! ```
//!
//! ## Quoting rules (RFC 4180)
//!
//! - Wrap a field in `"..."` if it contains `,`, `"`, `\r`, or `\n`
//! - Inside a quoted field, `"` is doubled (`""`)
//! - Plain ASCII without special chars goes unquoted
//!
//! ## Common use cases
//!
//! - Admin "Export to CSV" buttons (large querysets)
//! - Logs / audit trail dumps
//! - Bulk data download endpoints

/// CSV writer that builds output into an in-memory `String`.
///
/// For very large exports, write rows in batches and flush — but the typical
/// admin-export-button pattern fits comfortably in memory.
#[derive(Default)]
pub struct CsvWriter {
    out: String,
    column_count: Option<usize>,
}

impl CsvWriter {
    /// New empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Write the header row. Pins the column count — subsequent `row()`
    /// calls must match (extra fields truncated, short rows padded).
    pub fn headers<I, S>(&mut self, headers: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let headers: Vec<String> = headers.into_iter().map(|s| s.as_ref().to_owned()).collect();
        self.column_count = Some(headers.len());
        self.write_row(&headers);
    }

    /// Write a data row. If `headers()` was called, the row is padded
    /// (with empty strings) or truncated to match the column count.
    pub fn row<I, S>(&mut self, row: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut row: Vec<String> = row.into_iter().map(|s| s.as_ref().to_owned()).collect();
        if let Some(n) = self.column_count {
            row.resize(n, String::new());
        }
        self.write_row(&row);
    }

    /// Take the buffered CSV output, consuming the writer.
    #[must_use]
    pub fn into_string(self) -> String {
        self.out
    }

    /// View the current buffered CSV output without consuming the writer.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.out
    }

    fn write_row(&mut self, row: &[String]) {
        for (i, field) in row.iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            self.out.push_str(&escape_field(field));
        }
        self.out.push_str("\r\n");
    }
}

/// Escape one CSV field per RFC 4180.
fn escape_field(s: &str) -> String {
    let needs_quoting = s.bytes().any(|b| matches!(b, b',' | b'"' | b'\r' | b'\n'));
    if !needs_quoting {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fields_unquoted() {
        let mut w = CsvWriter::new();
        w.row(&["a", "b", "c"]);
        assert_eq!(w.as_str(), "a,b,c\r\n");
    }

    #[test]
    fn comma_in_field_gets_quoted() {
        let mut w = CsvWriter::new();
        w.row(&["a", "b, c", "d"]);
        assert_eq!(w.as_str(), "a,\"b, c\",d\r\n");
    }

    #[test]
    fn quote_in_field_doubled_and_quoted() {
        let mut w = CsvWriter::new();
        w.row(&["a", r#"say "hi""#, "b"]);
        assert_eq!(w.as_str(), "a,\"say \"\"hi\"\"\",b\r\n");
    }

    #[test]
    fn newline_in_field_gets_quoted() {
        let mut w = CsvWriter::new();
        w.row(&["a", "line1\nline2", "b"]);
        assert_eq!(w.as_str(), "a,\"line1\nline2\",b\r\n");
    }

    #[test]
    fn carriage_return_in_field_gets_quoted() {
        let mut w = CsvWriter::new();
        w.row(&["a", "line1\rline2", "b"]);
        assert!(w.as_str().contains("\"line1\rline2\""));
    }

    #[test]
    fn empty_field_unquoted() {
        let mut w = CsvWriter::new();
        w.row(&["", "x", ""]);
        assert_eq!(w.as_str(), ",x,\r\n");
    }

    #[test]
    fn headers_then_rows() {
        let mut w = CsvWriter::new();
        w.headers(&["id", "name"]);
        w.row(&["1", "Alice"]);
        w.row(&["2", "Bob"]);
        assert_eq!(w.into_string(), "id,name\r\n1,Alice\r\n2,Bob\r\n");
    }

    #[test]
    fn row_padded_to_column_count_after_headers() {
        let mut w = CsvWriter::new();
        w.headers(&["a", "b", "c"]);
        w.row(&["1"]); // short
        assert_eq!(w.into_string(), "a,b,c\r\n1,,\r\n");
    }

    #[test]
    fn row_truncated_to_column_count_after_headers() {
        let mut w = CsvWriter::new();
        w.headers(&["a", "b"]);
        w.row(&["1", "2", "3", "4"]); // long
        assert_eq!(w.into_string(), "a,b\r\n1,2\r\n");
    }

    #[test]
    fn escape_field_simple() {
        assert_eq!(escape_field("plain"), "plain");
        assert_eq!(escape_field("a,b"), "\"a,b\"");
        assert_eq!(escape_field("say \"x\""), "\"say \"\"x\"\"\"");
    }
}
