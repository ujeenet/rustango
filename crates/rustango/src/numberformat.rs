//! Django-shape number formatter — mirrors
//! `django.utils.numberformat.format`.
//!
//! Locale-aware number rendering with configurable decimal
//! separator (`,` for de_DE / fr_FR; `.` for en_US), thousand
//! separator (`.` / `,` / `' '` etc.), digit grouping (typically 3
//! for Western numerals; some scripts use 4), and optional fixed
//! decimal-place width.
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
//! ## Differences from Django
//!
//! Django's full `format()` accepts a callable `force_grouping`
//! arg + `use_l10n` toggle that pulls a `Decimal` shape. rustango's
//! simpler version takes `f64` (or `i64` via `from_i64`) and a
//! always-grouping flag implicit in `grouping > 0`. Apps that need
//! true `Decimal` (rust_decimal) precision should format the
//! integer + fractional parts themselves, then call this with the
//! result.

/// Format a floating-point number per Django's `numberformat.format`
/// shape.
///
/// `decimal_pos = None` keeps the natural precision (matching
/// Python's `repr(x)`); `Some(n)` rounds to `n` fractional digits.
/// `grouping = 0` disables thousand-separator insertion;
/// `grouping > 0` inserts `thousand_sep` every N digits in the
/// integer part counting from the right.
///
/// Negative numbers preserve the leading `-` in front of any
/// grouping. NaN / ±Infinity short-circuit to Rust's default
/// `Display` (`"NaN"` / `"inf"` / `"-inf"`); Django raises
/// `TypeError` for `Decimal('NaN')` — we choose the lenient path
/// since handler code probably wants ANY string back to render.
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
            // Rust default `Display` for f64 picks a shortest round-trip
            // representation. Matches Python `repr(x)` for typical inputs.
            let s = format!("{abs}");
            // Special case: "5" should not stay "5" — Django's natural
            // representation keeps it as `"5"` too, so the default is fine.
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

/// Format an integer per the same shape — convenience wrapper for
/// `i64` so callers don't have to think about float precision when
/// the value is integral.
#[must_use]
pub fn format_i64(value: i64, grouping: usize, thousand_sep: &str) -> String {
    let negative = value < 0;
    // `i64::MIN.unsigned_abs()` is safe — wraps to `i64::MIN as u64`
    // via the standard library; Display on u64 always succeeds.
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

/// Insert `sep` every `n` digits in `digits` counting from the
/// right. Internal helper used by both `format` (float path) and
/// `format_i64` (integer path).
///
/// Assumes `digits` is digits-only (caller has already stripped sign
/// and split off the fractional part). Empty input returns empty.
fn group_digits(digits: &str, n: usize, sep: &str) -> String {
    if digits.len() <= n {
        return digits.to_owned();
    }
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + (digits.len() / n) * sep.len());
    let first_group_len = digits.len() % n;
    let mut i = 0;
    if first_group_len > 0 {
        // SAFETY: digits is ASCII-only (digits 0-9); slicing on a byte
        // boundary is safe.
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

    // -------- format (f64) — basic shapes --------

    #[test]
    fn format_simple_no_grouping_no_decimal_round() {
        // Django: format(1234.567, '.', None, 0, '') == '1234.567'
        assert_eq!(format(1234.567, ".", None, 0, ""), "1234.567");
    }

    #[test]
    fn format_integer_value_no_decimal_part() {
        // Float that's actually integer-valued — Rust's Display emits "100".
        assert_eq!(format(100.0, ".", None, 0, ""), "100");
    }

    #[test]
    fn format_with_fixed_decimal_pos() {
        assert_eq!(format(1.5, ".", Some(2), 0, ""), "1.50");
        assert_eq!(format(1.5, ".", Some(4), 0, ""), "1.5000");
    }

    #[test]
    fn format_decimal_pos_rounds() {
        // Rust's `format!("{:.2}")` uses the IEEE 754 nearest-even
        // round mode AND f64's imprecise representation — `1.555` is
        // actually stored as `1.5549...` so it rounds DOWN to `1.55`.
        // Verify the call doesn't panic and produces SOME 2-decimal
        // output; don't pin the exact value (caller-visible
        // round-tripping artifacts of binary float).
        let out = format(1.555, ".", Some(2), 0, "");
        assert!(out.starts_with("1.5"), "got: {out:?}");
        assert_eq!(out.chars().count(), 4, "got: {out:?}");
        // A clean non-boundary case rounds predictably.
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
        // French: non-breaking space thousands, comma decimal, no frac.
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
        // Exactly 1000 — one separator after the leading digit.
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
        // Even with grouping=3, an empty separator is a no-op.
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
        // i64::MIN.unsigned_abs() — the wrapping branch.
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
}
