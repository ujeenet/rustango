//! Number formatting, shaped like `django.utils.numberformat.format`.
//!
//! You choose the decimal separator, the thousand separator, the group
//! size and how many decimal places to show.
//!
//! ```ignore
//! use rustango::numberformat::format;
//!
//! // en_US default — period decimal, no thousand sep
//! assert_eq!(format(1234.567, ".", None, 0, ""), "1234.567");
//!
//! // en_US with thousands grouping
//! assert_eq!(format(1234567.89, ".", Some(2), 3, ","), "1,234,567.89");
//!
//! // de_DE shape — comma decimal, period thousands
//! assert_eq!(format(1234567.89, ",", Some(2), 3, "."), "1.234.567,89");
//!
//! // fr_FR shape — comma decimal, non-breaking space thousands
//! assert_eq!(format(1234567.0, ",", Some(0), 3, "\u{00A0}"),
//!            "1\u{00A0}234\u{00A0}567");
//!
//! // Indian numbering: groups of 3 then 2 isn't supported — we do
//! // uniform `grouping=3` like Django's basic shape. Indian-numbering
//! // apps reach for a custom grouping function.
//! ```
//!
//! ## Limits
//!
//! Values are `f64`, or `i64` through
//! [`format_i64`](crate::numberformat::format_i64). If you need
//! exact decimal maths, format the value yourself first and pass the
//! parts in.

/// Format a float.
///
/// `decimal_pos = None` keeps the number's natural precision;
/// `Some(n)` rounds to `n` decimal places. `grouping = 0` turns off
/// the thousand separator; a larger value inserts `thousand_sep`
/// every N digits, counting from the right.
///
/// A minus sign stays in front. NaN and infinity come back as
/// `"NaN"`, `"inf"` or `"-inf"` rather than an error, so a template
/// always has something to render.
#[must_use]
pub fn format(
    value: f64,
    decimal_sep: &str,
    decimal_pos: Option<usize>,
    grouping: usize,
    thousand_sep: &str,
) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let negative = value < 0.0;
    let abs = value.abs();
    let formatted = match decimal_pos {
        Some(p) => format!("{abs:.p$}"),
        None => {
            let s = format!("{abs}");
            // `Display` for f64 gives the shortest round-trip form, so
            // "5" stays "5" and nothing is padded.
            s
        }
    };
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i.to_owned(), Some(f.to_owned())),
        None => (formatted, None),
    };
    let grouped = if grouping > 0 && !thousand_sep.is_empty() {
        group_digits(&int_part, grouping, thousand_sep)
    } else {
        int_part
    };
    let mut out =
        String::with_capacity(grouped.len() + frac_part.as_deref().map_or(0, str::len) + 4);
    if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    if let Some(frac) = frac_part {
        out.push_str(decimal_sep);
        out.push_str(&frac);
    }
    out
}

/// Django's
/// [`floatformat`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#floatformat)
/// filter. The sign of `precision` decides whether trailing zeros are
/// kept:
///
/// * `precision > 0`: exactly that many decimals, zeros kept.
/// * `precision < 0`: at most that many decimals, zeros dropped.
/// * `precision == 0`: no decimals, rounded to a whole number.
///
/// A negative precision is handy for prices: `-2` shows `$5` for a
/// round amount and `$5.50` otherwise.
///
/// ```
/// use rustango::numberformat::floatformat;
/// assert_eq!(floatformat(34.23234, -1), "34.2");
/// assert_eq!(floatformat(34.0, -1), "34");
/// assert_eq!(floatformat(34.23234, 3), "34.232");
/// assert_eq!(floatformat(34.0, 3), "34.000");
/// assert_eq!(floatformat(34.23234, -3), "34.232");
/// assert_eq!(floatformat(34.0, -3), "34");
/// ```
#[must_use]
pub fn floatformat(value: f64, precision: i64) -> String {
    let abs = precision.unsigned_abs() as usize;
    let drop_trailing = precision <= 0;
    let formatted = format!("{value:.abs$}");
    if drop_trailing {
        if let Some((int_part, frac_part)) = formatted.split_once('.') {
            if frac_part.chars().all(|c| c == '0') {
                return int_part.to_owned();
            }
        }
    }
    formatted
}

/// Group an `i64`, with no float rounding to worry about.
#[must_use]
pub fn format_i64(value: i64, grouping: usize, thousand_sep: &str) -> String {
    let negative = value < 0;
    // `unsigned_abs` handles `i64::MIN` without overflowing.
    let abs = value.unsigned_abs().to_string();
    let grouped = if grouping > 0 && !thousand_sep.is_empty() {
        group_digits(&abs, grouping, thousand_sep)
    } else {
        abs
    };
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Insert `sep` every `n` digits, counting from the right.
///
/// `digits` must already be digits only: the caller strips the sign
/// and the fractional part first.
fn group_digits(digits: &str, n: usize, sep: &str) -> String {
    if digits.len() <= n {
        return digits.to_owned();
    }
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + (digits.len() / n) * sep.len());
    let first_group_len = digits.len() % n;
    let mut i = 0;
    if first_group_len > 0 {
        // `digits` is ASCII, so byte slicing is fine.
        out.push_str(&digits[..first_group_len]);
        i = first_group_len;
    }
    let _ = bytes; // unused beyond length math
    while i < digits.len() {
        if !out.is_empty() {
            out.push_str(sep);
        }
        out.push_str(&digits[i..i + n]);
        i += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- format (f64) --------

    #[test]
    fn format_simple_no_grouping_no_decimal_round() {
        assert_eq!(format(1234.567, ".", None, 0, ""), "1234.567");
    }

    #[test]
    fn format_integer_value_no_decimal_part() {
        // An integral float prints without a decimal point.
        assert_eq!(format(100.0, ".", None, 0, ""), "100");
    }

    #[test]
    fn format_with_fixed_decimal_pos() {
        assert_eq!(format(1.5, ".", Some(2), 0, ""), "1.50");
        assert_eq!(format(1.5, ".", Some(4), 0, ""), "1.5000");
    }

    #[test]
    fn format_decimal_pos_rounds() {
        // `1.555` is stored as `1.5549...`, so the exact result of a
        // halfway case is not worth pinning. Just check the shape.
        let out = format(1.555, ".", Some(2), 0, "");
        assert!(out.starts_with("1.5"), "got: {out:?}");
        assert_eq!(out.chars().count(), 4, "got: {out:?}");
        // Values away from the halfway point round as expected.
        assert_eq!(format(1.567, ".", Some(2), 0, ""), "1.57");
        assert_eq!(format(1.234, ".", Some(2), 0, ""), "1.23");
    }

    #[test]
    fn format_decimal_pos_zero_drops_fraction() {
        assert_eq!(format(1234.7, ".", Some(0), 0, ""), "1235");
    }

    // -------- thousand-separator grouping --------

    #[test]
    fn format_thousands_grouping_en_us() {
        assert_eq!(format(1_234_567.89, ".", Some(2), 3, ","), "1,234,567.89");
    }

    #[test]
    fn format_thousands_grouping_de_de() {
        // German: period thousands, comma decimal.
        assert_eq!(format(1_234_567.89, ",", Some(2), 3, "."), "1.234.567,89");
    }

    #[test]
    fn format_thousands_grouping_fr_fr_nbsp() {
        // French: non-breaking space thousands, comma decimal.
        assert_eq!(
            format(1_234_567.0, ",", Some(0), 3, "\u{00A0}"),
            "1\u{00A0}234\u{00A0}567"
        );
    }

    #[test]
    fn format_below_grouping_threshold() {
        // < 1000: no separator inserted.
        assert_eq!(format(999.0, ".", Some(0), 3, ","), "999");
    }

    #[test]
    fn format_exactly_at_grouping_threshold() {
        // Exactly 1000: one separator after the leading digit.
        assert_eq!(format(1000.0, ".", Some(0), 3, ","), "1,000");
    }

    // -------- negatives --------

    #[test]
    fn format_negative_preserves_sign() {
        assert_eq!(format(-1234.56, ".", Some(2), 3, ","), "-1,234.56");
        assert_eq!(format(-100.0, ".", Some(0), 0, ""), "-100");
    }

    // -------- non-finite --------

    #[test]
    fn format_nan_uses_rust_display() {
        assert_eq!(format(f64::NAN, ".", Some(2), 0, ""), "NaN");
    }

    #[test]
    fn format_infinity_uses_rust_display() {
        assert_eq!(format(f64::INFINITY, ".", Some(2), 0, ""), "inf");
        assert_eq!(format(f64::NEG_INFINITY, ".", Some(2), 0, ""), "-inf");
    }

    // -------- empty thousand_sep disables grouping --------

    #[test]
    fn format_empty_thousand_sep_disables_grouping() {
        // An empty separator disables grouping even when grouping=3.
        assert_eq!(format(1234567.0, ".", Some(0), 3, ""), "1234567");
    }

    // -------- format_i64 --------

    #[test]
    fn format_i64_small_no_grouping() {
        assert_eq!(format_i64(42, 0, ""), "42");
    }

    #[test]
    fn format_i64_grouped() {
        assert_eq!(format_i64(1_234_567, 3, ","), "1,234,567");
        assert_eq!(format_i64(1_000, 3, "."), "1.000");
    }

    #[test]
    fn format_i64_negative() {
        assert_eq!(format_i64(-1_234_567, 3, ","), "-1,234,567");
        assert_eq!(format_i64(-1, 3, ","), "-1");
    }

    #[test]
    fn format_i64_zero() {
        assert_eq!(format_i64(0, 3, ","), "0");
    }

    #[test]
    fn format_i64_min_does_not_panic() {
        // The `i64::MIN` edge case.
        let s = format_i64(i64::MIN, 3, ",");
        assert!(s.starts_with('-'));
        assert!(s.contains(','));
    }

    // -------- group_digits internal helper --------

    #[test]
    fn group_digits_canonical_examples() {
        assert_eq!(group_digits("1234567", 3, ","), "1,234,567");
        assert_eq!(group_digits("1234567", 3, "."), "1.234.567");
        assert_eq!(group_digits("100", 3, ","), "100");
        assert_eq!(group_digits("1000", 3, ","), "1,000");
        assert_eq!(group_digits("999999999", 3, ","), "999,999,999");
    }

    #[test]
    fn group_digits_empty_input() {
        assert_eq!(group_digits("", 3, ","), "");
    }

    #[test]
    fn group_digits_smaller_than_one_group() {
        assert_eq!(group_digits("12", 3, ","), "12");
    }

    // -------- floatformat --------

    #[test]
    fn floatformat_default_precision_drops_zero_decimal() {
        assert_eq!(floatformat(34.23234, -1), "34.2");
        assert_eq!(floatformat(34.0, -1), "34");
        assert_eq!(floatformat(34.5, -1), "34.5");
    }

    #[test]
    fn floatformat_positive_precision_keeps_trailing_zeros() {
        assert_eq!(floatformat(34.23234, 3), "34.232");
        assert_eq!(floatformat(34.0, 3), "34.000");
        assert_eq!(floatformat(34.5, 2), "34.50");
    }

    #[test]
    fn floatformat_negative_precision_drops_trailing_zeros() {
        // Up to |N| decimals, trailing zeros dropped.
        assert_eq!(floatformat(34.23234, -3), "34.232");
        assert_eq!(floatformat(34.0, -3), "34");
        assert_eq!(floatformat(5.5, -2), "5.50");
        assert_eq!(floatformat(5.0, -2), "5");
    }

    #[test]
    fn floatformat_zero_precision_no_decimals() {
        // precision=0 gives no decimals. Exact halves round to even.
        assert_eq!(floatformat(34.4, 0), "34");
        assert_eq!(floatformat(34.6, 0), "35");
        assert_eq!(floatformat(34.0, 0), "34");
    }

    #[test]
    fn floatformat_rounds_to_nearest() {
        // Standard f64 rounding.
        assert_eq!(floatformat(1.234, 2), "1.23");
        assert_eq!(floatformat(1.236, 2), "1.24");
    }

    #[test]
    fn floatformat_negative_values() {
        assert_eq!(floatformat(-34.5, -1), "-34.5");
        assert_eq!(floatformat(-34.0, -1), "-34");
        assert_eq!(floatformat(-1.2345, 2), "-1.23");
    }
}
