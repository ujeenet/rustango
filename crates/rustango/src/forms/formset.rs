//! Form sets — Django's `formset_factory` / `modelformset_factory`
//! shape. Issue #49.
//!
//! Parse N copies of the same [`crate::forms::Form`] from a single
//! HTTP request payload. Each row is keyed
//! `<prefix>-<N>-<field>` (Django convention: prefix `form` by
//! default, so `form-0-title`, `form-1-title`, `form-2-title` for
//! three rows of a `Title` field).
//!
//! ```ignore
//! use rustango::forms::formset::parse_formset;
//!
//! #[derive(rustango::Form)]
//! struct ArticleRow {
//!     title: String,
//!     #[rustango(min = 0)]
//!     order: i32,
//! }
//!
//! async fn save_articles(axum::Form(data): axum::Form<HashMap<String, String>>) {
//!     match parse_formset::<ArticleRow>(&data, "form") {
//!         Ok(rows) => /* save Vec<ArticleRow> */,
//!         Err(errors) => /* errors[i] is the FormErrors for row i */,
//!     }
//! }
//! ```
//!
//! ## Management form
//!
//! Django requires every formset payload to carry a "management
//! form" — three integer fields that bound the row range:
//!
//! - `<prefix>-TOTAL_FORMS`: how many rows the client submitted.
//!   **Required** — `parse_formset` errors if missing or malformed.
//! - `<prefix>-INITIAL_FORMS`: how many rows existed when the page
//!   rendered (extras are "new" rows). Currently unused by the
//!   parser; preserved for round-tripping.
//! - `<prefix>-MAX_NUM_FORMS`: upper bound. Unused by the parser;
//!   the caller enforces it.
//!
//! The parser only needs `TOTAL_FORMS`. The rest are accepted (for
//! future use) but not consulted.
//!
//! ## Errors
//!
//! Each row's [`crate::forms::FormErrors`] lands at the matching
//! index in the returned `Vec<FormErrors>`. A row with no errors
//! is represented by an empty `FormErrors`. A separate top-level
//! error variant covers the malformed-management-form case.

use std::collections::HashMap;

use super::{Form, FormErrors};

/// Error returned when the management form is missing or malformed.
/// Distinct from per-row errors so the caller can give a clear
/// "your form rendering is broken" message rather than blaming the
/// user's input.
#[derive(Debug, thiserror::Error)]
pub enum FormSetError {
    /// `<prefix>-TOTAL_FORMS` was absent. The form likely didn't
    /// render through the formset's management-form helper.
    #[error("formset: missing `{0}-TOTAL_FORMS` management field")]
    MissingTotalForms(String),
    /// `<prefix>-TOTAL_FORMS` was present but not a non-negative
    /// integer.
    #[error("formset: `{prefix}-TOTAL_FORMS` is not a non-negative integer (got `{got}`)")]
    InvalidTotalForms { prefix: String, got: String },
}

/// Pull the `TOTAL_FORMS` count out of the payload. Returns
/// [`FormSetError`] if the field is missing or malformed.
pub fn total_forms(data: &HashMap<String, String>, prefix: &str) -> Result<usize, FormSetError> {
    let key = format!("{prefix}-TOTAL_FORMS");
    let raw = data
        .get(&key)
        .ok_or_else(|| FormSetError::MissingTotalForms(prefix.to_owned()))?;
    raw.parse::<usize>()
        .map_err(|_| FormSetError::InvalidTotalForms {
            prefix: prefix.to_owned(),
            got: raw.clone(),
        })
}

/// Extract the per-row payload at index `idx`. Strips the
/// `<prefix>-<idx>-` prefix from each key. Returns a fresh HashMap
/// suitable for `F::parse`.
#[must_use]
pub fn row_payload(
    data: &HashMap<String, String>,
    prefix: &str,
    idx: usize,
) -> HashMap<String, String> {
    let row_prefix = format!("{prefix}-{idx}-");
    let mut out = HashMap::new();
    for (k, v) in data {
        if let Some(field) = k.strip_prefix(&row_prefix) {
            out.insert(field.to_owned(), v.clone());
        }
    }
    out
}

/// Parse a formset payload into `Vec<F>`, one entry per row.
///
/// On success: every row parsed cleanly, `Vec<F>` length equals
/// `<prefix>-TOTAL_FORMS`.
///
/// On per-row failure: the returned `Vec<FormErrors>` has length
/// `TOTAL_FORMS`; rows that parsed successfully carry an empty
/// `FormErrors`, rows that failed carry their actual errors. So
/// the caller can re-render the form with errors threaded back
/// to the right row.
///
/// # Errors
/// - [`FormSetError`] if the management form is missing/malformed.
/// - `Err(Err(Vec<FormErrors>))` if ANY row failed parsing.
pub fn parse_formset<F: Form>(
    data: &HashMap<String, String>,
    prefix: &str,
) -> Result<Result<Vec<F>, Vec<FormErrors>>, FormSetError> {
    let n = total_forms(data, prefix)?;
    let mut rows: Vec<F> = Vec::with_capacity(n);
    let mut errors: Vec<FormErrors> = Vec::with_capacity(n);
    let mut any_err = false;
    for idx in 0..n {
        let row_data = row_payload(data, prefix, idx);
        match F::parse(&row_data) {
            Ok(row) => {
                rows.push(row);
                errors.push(FormErrors::default());
            }
            Err(e) => {
                any_err = true;
                errors.push(e);
            }
        }
    }
    if any_err {
        Ok(Err(errors))
    } else {
        Ok(Ok(rows))
    }
}

/// Render the three management-form `<input type="hidden">` fields
/// callers stamp into their form's HTML. Counterpart to the parser
/// — emit this in the template so the next submit matches.
///
/// ```ignore
/// // In your handler:
/// ctx.insert("formset_management", &management_form_html("form", initial, total));
///
/// // In the template:
/// <form method="post">
///   {{ formset_management | safe }}
///   {% for row in rows %}
///     {{ row.title.field | safe }}
///   {% endfor %}
/// </form>
/// ```
///
/// `total` is "rows currently rendered" (TOTAL_FORMS); `initial` is
/// "rows that existed when the page first loaded" (INITIAL_FORMS).
/// They're usually equal on first render; they diverge when JS
/// adds extra-row inputs.
#[must_use]
pub fn management_form_html(prefix: &str, initial: usize, total: usize) -> String {
    format!(
        concat!(
            r#"<input type="hidden" name="{p}-TOTAL_FORMS" value="{t}">"#,
            r#"<input type="hidden" name="{p}-INITIAL_FORMS" value="{i}">"#,
            r#"<input type="hidden" name="{p}-MIN_NUM_FORMS" value="0">"#,
            r#"<input type="hidden" name="{p}-MAX_NUM_FORMS" value="1000">"#,
        ),
        p = prefix,
        t = total,
        i = initial,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forms::{Form, FormErrors};
    use std::collections::HashMap;

    // ---------- management form ----------

    #[test]
    fn total_forms_returns_count() {
        let mut data = HashMap::new();
        data.insert("form-TOTAL_FORMS".into(), "3".into());
        assert_eq!(total_forms(&data, "form").unwrap(), 3);
    }

    #[test]
    fn total_forms_missing_errors() {
        let data: HashMap<String, String> = HashMap::new();
        let r = total_forms(&data, "form");
        assert!(matches!(r, Err(FormSetError::MissingTotalForms(_))));
    }

    #[test]
    fn total_forms_malformed_errors() {
        let mut data = HashMap::new();
        data.insert("form-TOTAL_FORMS".into(), "not-a-number".into());
        let r = total_forms(&data, "form");
        assert!(matches!(r, Err(FormSetError::InvalidTotalForms { .. })));
    }

    #[test]
    fn total_forms_supports_custom_prefix() {
        let mut data = HashMap::new();
        data.insert("rows-TOTAL_FORMS".into(), "5".into());
        assert_eq!(total_forms(&data, "rows").unwrap(), 5);
    }

    // ---------- row_payload ----------

    #[test]
    fn row_payload_strips_prefix_and_index() {
        let mut data = HashMap::new();
        data.insert("form-0-title".into(), "Hello".into());
        data.insert("form-0-body".into(), "World".into());
        data.insert("form-1-title".into(), "Second".into());
        let row = row_payload(&data, "form", 0);
        assert_eq!(row.get("title").map(String::as_str), Some("Hello"));
        assert_eq!(row.get("body").map(String::as_str), Some("World"));
        assert!(!row.contains_key("form-0-title"));
        assert!(!row.contains_key("Second")); // other rows excluded
    }

    #[test]
    fn row_payload_for_missing_index_is_empty() {
        let mut data = HashMap::new();
        data.insert("form-0-title".into(), "x".into());
        let row = row_payload(&data, "form", 99);
        assert!(row.is_empty());
    }

    // ---------- parse_formset ----------

    /// Test form that requires `title` to be non-empty.
    #[derive(Debug, PartialEq, Eq)]
    struct TitleRow {
        title: String,
    }
    impl Form for TitleRow {
        fn parse(data: &HashMap<String, String>) -> Result<Self, FormErrors> {
            let mut errs = FormErrors::default();
            let title = match data.get("title") {
                Some(s) if !s.is_empty() => s.clone(),
                _ => {
                    errs.add("title", "required");
                    String::new()
                }
            };
            if errs.is_empty() {
                Ok(Self { title })
            } else {
                Err(errs)
            }
        }
    }

    #[test]
    fn parse_formset_all_rows_succeed() {
        let mut data = HashMap::new();
        data.insert("form-TOTAL_FORMS".into(), "2".into());
        data.insert("form-0-title".into(), "First".into());
        data.insert("form-1-title".into(), "Second".into());
        let outcome = parse_formset::<TitleRow>(&data, "form").unwrap();
        let rows = outcome.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "First");
        assert_eq!(rows[1].title, "Second");
    }

    #[test]
    fn parse_formset_per_row_errors_threaded_by_index() {
        let mut data = HashMap::new();
        data.insert("form-TOTAL_FORMS".into(), "3".into());
        // idx 0: missing title → row 0 errors.
        data.insert("form-1-title".into(), "ok".into());
        // idx 2: missing title → row 2 errors.
        let outcome = parse_formset::<TitleRow>(&data, "form").unwrap();
        let errors = outcome.unwrap_err();
        assert_eq!(errors.len(), 3);
        assert!(!errors[0].is_empty(), "row 0 should have errors");
        assert!(errors[1].is_empty(), "row 1 parsed cleanly");
        assert!(!errors[2].is_empty(), "row 2 should have errors");
    }

    #[test]
    fn parse_formset_zero_rows() {
        let mut data = HashMap::new();
        data.insert("form-TOTAL_FORMS".into(), "0".into());
        let outcome = parse_formset::<TitleRow>(&data, "form").unwrap();
        let rows = outcome.unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_formset_missing_management_errors() {
        let data: HashMap<String, String> = HashMap::new();
        let r = parse_formset::<TitleRow>(&data, "form");
        assert!(matches!(r, Err(FormSetError::MissingTotalForms(_))));
    }

    // ---------- management_form_html ----------

    #[test]
    fn management_form_html_contains_all_four_fields() {
        let html = management_form_html("form", 2, 3);
        assert!(html.contains(r#"name="form-TOTAL_FORMS" value="3""#));
        assert!(html.contains(r#"name="form-INITIAL_FORMS" value="2""#));
        assert!(html.contains(r#"name="form-MIN_NUM_FORMS""#));
        assert!(html.contains(r#"name="form-MAX_NUM_FORMS""#));
    }

    #[test]
    fn management_form_html_round_trips_through_parser() {
        // Render with 4 total, then parse the management form back.
        let html = management_form_html("custom", 1, 4);
        // The HTML form would submit these as form-encoded values;
        // we simulate that by extracting the totals.
        assert!(html.contains(r#"name="custom-TOTAL_FORMS" value="4""#));
        let mut data = HashMap::new();
        data.insert("custom-TOTAL_FORMS".into(), "4".into());
        assert_eq!(total_forms(&data, "custom").unwrap(), 4);
    }
}
