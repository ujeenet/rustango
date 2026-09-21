//! HTTP date parser and formatter.
//!
//! Use it for `Last-Modified`, `If-Modified-Since`, `Expires`, `Date` and
//! other HTTP date headers.
//!
//! RFC 7231 §7.1.1.1 says a server must send IMF-fixdate but must
//! accept all three old formats:
//!
//! * **IMF-fixdate** (RFC 1123, the one to send):
//!   `"Sun, 06 Nov 1994 08:49:37 GMT"`
//! * **RFC 850**, obsolete: `"Sunday, 06-Nov-94 08:49:37 GMT"`
//! * **ANSI C asctime()**, no zone: `"Sun Nov  6 08:49:37 1994"`
//!
//! ```ignore
//! use rustango::http_date::{http_date, parse_http_date};
//!
//! // Format current epoch seconds as IMF-fixdate.
//! let header = http_date(0);
//! assert_eq!(header, "Thu, 01 Jan 1970 00:00:00 GMT");
//!
//! // Parse all three legacy shapes.
//! assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
//! assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), Some(784111777));
//! assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(784111777));
//! ```

/// Format Unix seconds as an IMF-fixdate header, like
/// `"Sun, 06 Nov 1994 08:49:37 GMT"`.
///
/// The output is always GMT, as RFC 7231 requires, so convert a local
/// time before calling.
///
/// A value too large for `i64` falls back to the epoch, and a value
/// outside chrono's range falls back to now. Both beat a panic in a
/// `Last-Modified` header.
#[must_use]
pub fn http_date(secs: u64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(secs).unwrap_or(0), 0)
        .unwrap_or_else(chrono::Utc::now);
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Parse any of the three HTTP date shapes from RFC 7231 §7.1.1.1
/// into Unix seconds.
///
/// Returns `None` for empty input, for a shape we do not recognise,
/// and for dates before 1970.
///
/// A two-digit RFC 850 year follows chrono's split: `00..=68` is
/// 2000-2068 and `69..=99` is 1969-1999.
///
/// ```ignore
/// use rustango::http_date::parse_http_date;
/// assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
/// assert!(parse_http_date("not a date").is_none());
/// ```
#[must_use]
pub fn parse_http_date(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 1. IMF-fixdate (RFC 1123): "Sun, 06 Nov 1994 08:49:37 GMT"
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%a, %d %b %Y %H:%M:%S GMT") {
        return positive_secs(naive);
    }
    // 2. RFC 2822: "Sun, 06 Nov 1994 08:49:37 +0000". Kept because the
    //    static-files parser accepted it before this module existed.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        let ts = dt.timestamp();
        return if ts >= 0 {
            u64::try_from(ts).ok()
        } else {
            None
        };
    }
    // 3. RFC 850: "Sunday, 06-Nov-94 08:49:37 GMT". Two-digit year,
    //    hyphens, full weekday name.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%A, %d-%b-%y %H:%M:%S GMT") {
        return positive_secs(naive);
    }
    // 4. ANSI C asctime(): "Sun Nov  6 08:49:37 1994". A one-digit day
    //    is space-padded, which `%e` accepts.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%a %b %e %H:%M:%S %Y") {
        return positive_secs(naive);
    }

    None
}

/// Turn a parsed UTC datetime into Unix seconds. Pre-1970 → `None`.
fn positive_secs(naive: chrono::NaiveDateTime) -> Option<u64> {
    let ts = naive.and_utc().timestamp();
    if ts < 0 {
        None
    } else {
        u64::try_from(ts).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- http_date (format) --------

    #[test]
    fn format_epoch_zero_is_unix_epoch_imf_fixdate() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn format_canonical_rfc_example() {
        // 784111777 = Sun, 06 Nov 1994 08:49:37 GMT (RFC 7231 example).
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn format_always_ends_with_gmt() {
        // RFC 7231 mandates "GMT" as the zone tag, not "+0000" / "UTC".
        for ts in [0u64, 100_000, 1_000_000_000, 1_700_000_000] {
            let s = http_date(ts);
            assert!(s.ends_with(" GMT"), "{s:?} missing GMT tag");
        }
    }

    #[test]
    fn format_round_trips_through_parse() {
        for ts in [0u64, 1, 86_400, 1_000_000, 1_700_000_000] {
            let s = http_date(ts);
            assert_eq!(parse_http_date(&s), Some(ts), "round-trip failed at {ts}");
        }
    }

    // -------- parse_http_date — IMF-fixdate (RFC 1123) --------

    #[test]
    fn parse_imf_fixdate() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    }

    // -------- parse_http_date — RFC 850 --------

    #[test]
    fn parse_rfc_850() {
        // Two-digit year, hyphens between date parts, full weekday name.
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
    }

    #[test]
    fn parse_rfc_850_y2k_rolls_after_69() {
        // chrono's `%y` split: `94` → 1994, `05` → 2005. Feb 3 2005
        // was a Thursday, and chrono checks the weekday.
        let ts_2005 = parse_http_date("Thursday, 03-Feb-05 12:00:00 GMT").unwrap();
        // 2005-02-03 12:00:00 UTC = 1107432000
        assert_eq!(ts_2005, 1_107_432_000);
    }

    // -------- parse_http_date — ANSI C asctime --------

    #[test]
    fn parse_asctime_single_digit_day() {
        // Day of month is `6` (one digit) with a leading space to pad.
        assert_eq!(
            parse_http_date("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
    }

    #[test]
    fn parse_asctime_two_digit_day() {
        assert_eq!(
            parse_http_date("Sat Dec 31 23:59:59 1994"),
            Some(788_918_399)
        );
    }

    // -------- parse_http_date — rejection cases --------

    #[test]
    fn parse_empty_is_none() {
        assert_eq!(parse_http_date(""), None);
        assert_eq!(parse_http_date("   "), None);
    }

    #[test]
    fn parse_garbage_is_none() {
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_http_date("1994-11-06T08:49:37Z"), None); // ISO 8601 not accepted by RFC 7231
        assert_eq!(parse_http_date("Sun, 06 Nov"), None); // truncated
    }

    #[test]
    fn parse_pre_1970_is_none() {
        // Pre-1970 dates are not representable here; return None.
        assert_eq!(parse_http_date("Mon, 01 Jan 1900 00:00:00 GMT"), None);
    }

    // -------- all three formats decode to the same instant --------

    #[test]
    fn all_three_formats_round_trip_to_same_epoch() {
        // The RFC 7231 example: one instant, three shapes.
        let imf = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        let rfc850 = parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT").unwrap();
        let asctime = parse_http_date("Sun Nov  6 08:49:37 1994").unwrap();
        assert_eq!(imf, rfc850);
        assert_eq!(imf, asctime);
        assert_eq!(imf, 784_111_777);
    }

    #[test]
    fn parse_accepts_surrounding_whitespace() {
        // Some servers leak a leading or trailing space.
        assert_eq!(
            parse_http_date("  Sun, 06 Nov 1994 08:49:37 GMT  "),
            Some(784_111_777)
        );
    }
}
