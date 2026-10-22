//! Reusable declarative field-constraint validators.
//!
//! Used by the `#[derive(Serializer)]` write path: each writable
//! field is checked against the constraints resolved for it — the
//! serializer-declared value when given
//! (`#[serializer(max_length = …)]`), otherwise the model's
//! [`crate::core::FieldSchema`] (`max_length` / `min` / `max` /
//! `choices`). The messages match the admin `DynamicForm`, so a
//! field reports the same wording wherever it is validated.

use super::FormErrors;
use serde_json::Value;

/// Validate a single field value against its resolved constraints,
/// appending any failures to `errors` keyed by `field`.
///
/// Type dispatch is by the JSON value, so the macro needs no Rust-type
/// introspection:
/// * **strings** — `max_length`, `min_length`, `choices`,
/// * **numbers** — `min`, `max`.
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
    } else if value.is_number() {
        // Integers compare exactly; floats (and u64 past i64::MAX) as f64.
        let below = |m: i64| match value.as_i64() {
            Some(n) => n < m,
            None => value.as_f64().is_some_and(|n| n < m as f64),
        };
        let above = |m: i64| match value.as_i64() {
            Some(n) => n > m,
            None => value.as_f64().is_some_and(|n| n > m as f64),
        };
        if let Some(m) = min.filter(|&m| below(m)) {
            errors.add(field, format!("Ensure this value is ≥ {m}."));
        }
        if let Some(m) = max.filter(|&m| above(m)) {
            errors.add(field, format!("Ensure this value is ≤ {m}."));
        }
    }
}

/// A type `#[serializer(max_length / min_length)]` can bound.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`max_length` / `min_length` need a string field, not `{Self}`"
)]
pub trait LengthBounded {}
impl LengthBounded for String {}
impl LengthBounded for str {}
impl LengthBounded for Box<str> {}
impl LengthBounded for std::borrow::Cow<'_, str> {}
impl<T: LengthBounded + ?Sized> LengthBounded for &T {}
impl<T: LengthBounded> LengthBounded for Option<T> {}

/// A type `#[serializer(min / max)]` can bound.
#[doc(hidden)]
#[diagnostic::on_unimplemented(message = "`min` / `max` need a numeric field, not `{Self}`")]
pub trait NumberBounded {}
macro_rules! number_bounded {
    ($($t:ty),*) => { $(impl NumberBounded for $t {})* };
}
number_bounded!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64);
impl<T: NumberBounded + ?Sized> NumberBounded for &T {}
impl<T: NumberBounded> NumberBounded for Option<T> {}

/// Compile-time check the derive emits for a length attribute.
#[doc(hidden)]
pub const fn assert_length_bounded<T: LengthBounded + ?Sized>() {}

/// Compile-time check the derive emits for a `min` / `max` attribute.
#[doc(hidden)]
pub const fn assert_number_bounded<T: NumberBounded + ?Sized>() {}

/// A bound attribute on a field type it cannot bound does not compile.
/// ```
/// # #[derive(rustango::Model, Clone)] #[rustango(table = "dv_row")]
/// # pub struct Row { #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>, pub n: f64, pub s: String }
/// #[derive(rustango::Serializer, serde::Deserialize, Default)]
/// #[serializer(model = Row)]
/// struct S {
///     #[serializer(min = 0)]
///     n: f64,
///     #[serializer(max_length = 3)]
///     s: String,
/// }
/// #[derive(rustango::Form)]
/// struct F {
///     #[form(min = 1)]
///     n: i64,
///     #[form(max_length = 3)]
///     s: String,
/// }
/// # fn main() {}
/// ```
/// ```compile_fail
/// # #[derive(rustango::Model, Clone)] #[rustango(table = "dv_row")]
/// # pub struct Row { #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>, pub n: f64, pub s: String }
/// #[derive(rustango::Serializer, serde::Deserialize, Default)]
/// #[serializer(model = Row)]
/// struct S {
///     #[serializer(max_length = 3)]
///     n: f64,
///     s: String,
/// }
/// # fn main() {}
/// ```
/// ```compile_fail
/// # #[derive(rustango::Model, Clone)] #[rustango(table = "dv_row")]
/// # pub struct Row { #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>, pub n: f64, pub s: String }
/// #[derive(rustango::Serializer, serde::Deserialize, Default)]
/// #[serializer(model = Row)]
/// struct S {
///     n: f64,
///     #[serializer(min = 0)]
///     s: String,
/// }
/// # fn main() {}
/// ```
/// ```compile_fail
/// #[derive(rustango::Form)]
/// struct F {
///     #[form(min = 1)]
///     s: String,
/// }
/// ```
/// ```compile_fail
/// #[derive(rustango::Form)]
/// struct F {
///     #[form(max_length = 3)]
///     n: i64,
/// }
/// ```
#[cfg(all(doctest, feature = "serializer"))]
pub struct BoundsMatchTheFieldType;

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
