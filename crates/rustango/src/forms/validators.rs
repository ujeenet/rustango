//! Reusable declarative field-constraint validators.
//!
//! Used by the `#[derive(Serializer)]` write path (DRF
//! `validators=[…]`): each writable field is checked against the
//! constraints resolved for it — the serializer-declared value when
//! given (`#[serializer(max_length = …)]`), otherwise the model's
//! [`crate::core::FieldSchema`] (`max_length` / `min` / `max` /
//! `choices`). The messages match the admin `DynamicForm` so every
//! validation surface speaks the same Django/DRF language.

use super::FormErrors;
use serde_json::Value;

/// Validate a single field value against its resolved constraints,
/// appending any failures to `errors` keyed by `field`.
///
/// Type dispatch is by the JSON value, so the macro needs no Rust-type
/// introspection:
/// * **strings** — `max_length`, `min_length`, `choices`,
/// * **integers** — `min`, `max`.
///
/// `null` / arrays / objects are skipped. `min_length` and `choices`
/// skip empty strings (an empty value means "not provided", caught by
/// NOT-NULL handling rather than a length/choice error) — matching
/// `DynamicForm`. String length is measured in characters
/// (`chars().count()`), matching `FieldSchema::max_length`'s
/// "characters" semantics.
#[allow(clippy::too_many_arguments)]
pub fn check_value(
    field: &str,
    value: &Value,
    max_length: Option<usize>,
    min_length: Option<usize>,
    min: Option<i64>,
    max: Option<i64>,
    choices: Option<&[(&str, &str)]>,
    errors: &mut FormErrors,
) {
    if let Some(s) = value.as_str() {
        let len = s.chars().count();
        if let Some(m) = max_length {
            if len > m {
                errors.add(
                    field,
                    format!("Ensure this value has at most {m} characters."),
                );
            }
        }
        if let Some(m) = min_length {
            if !s.is_empty() && len < m {
                errors.add(
                    field,
                    format!("Ensure this value has at least {m} characters."),
                );
            }
        }
        if let Some(cs) = choices {
            if !s.is_empty() && !cs.iter().any(|(v, _)| *v == s) {
                errors.add(field, "Select a valid choice.");
            }
        }
    } else if let Some(n) = value.as_i64() {
        if let Some(m) = min {
            if n < m {
                errors.add(field, format!("Ensure this value is ≥ {m}."));
            }
        }
        if let Some(m) = max {
            if n > m {
                errors.add(field, format!("Ensure this value is ≤ {m}."));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn max_length_flags_too_long_string() {
        let mut e = FormErrors::default();
        check_value(
            "name",
            &json!("abcdef"),
            Some(3),
            None,
            None,
            None,
            None,
            &mut e,
        );
        assert_eq!(e.get("name").len(), 1);
        assert!(e.get("name")[0].contains("at most 3 characters"));
    }

    #[test]
    fn min_length_skips_empty_but_flags_short() {
        let mut e = FormErrors::default();
        check_value("name", &json!(""), None, Some(3), None, None, None, &mut e);
        assert!(
            e.is_empty(),
            "empty string is 'not provided', not a length error"
        );
        check_value(
            "name",
            &json!("ab"),
            None,
            Some(3),
            None,
            None,
            None,
            &mut e,
        );
        assert!(e.get("name")[0].contains("at least 3 characters"));
    }

    #[test]
    fn min_max_bound_integers() {
        let mut e = FormErrors::default();
        check_value("n", &json!(5), None, None, Some(0), Some(3), None, &mut e);
        assert!(e.get("n")[0].contains("≤ 3"));
        let mut e = FormErrors::default();
        check_value("n", &json!(-1), None, None, Some(0), Some(3), None, &mut e);
        assert!(e.get("n")[0].contains("≥ 0"));
    }

    #[test]
    fn choices_rejects_value_not_in_set() {
        let mut e = FormErrors::default();
        let choices: &[(&str, &str)] = &[("draft", "Draft"), ("published", "Published")];
        check_value(
            "status",
            &json!("archived"),
            None,
            None,
            None,
            None,
            Some(choices),
            &mut e,
        );
        assert!(e.get("status")[0].contains("valid choice"));
        let mut e = FormErrors::default();
        check_value(
            "status",
            &json!("draft"),
            None,
            None,
            None,
            None,
            Some(choices),
            &mut e,
        );
        assert!(e.is_empty());
    }

    #[test]
    fn null_and_non_scalar_are_skipped() {
        let mut e = FormErrors::default();
        check_value(
            "x",
            &json!(null),
            Some(1),
            Some(1),
            Some(1),
            Some(1),
            None,
            &mut e,
        );
        check_value(
            "x",
            &json!({"a": 1}),
            Some(1),
            None,
            None,
            None,
            None,
            &mut e,
        );
        assert!(e.is_empty());
    }
}
