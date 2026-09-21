//! English month and weekday names, like `django.utils.dates`.
//!
//! Months take a 1-indexed number, weekdays take a
//! `chrono::Weekday`. The names are always English, which is what
//! `dateformat` expects; translation is handled elsewhere.
//!
//! ```
//! use chrono::Weekday;
//! use rustango::dates::{month_full, month_abbr, month_ap, weekday_full, weekday_abbr};
//!
//! assert_eq!(month_full(6), "June");
//! assert_eq!(month_abbr(6), "jun");
//! assert_eq!(month_ap(6), "June");
//! assert_eq!(month_ap(1), "Jan.");
//! assert_eq!(weekday_full(Weekday::Thu), "Thursday");
//! assert_eq!(weekday_abbr(Weekday::Thu), "Thu");
//! ```

use chrono::Weekday;

/// Full month name for `1..=12`, like Django's `MONTHS`. Any other
/// number gives `""`, so formatting code never panics.
#[must_use]
pub fn month_full(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "",
    }
}

/// Lowercase three-letter month name, `"jan"` to `"dec"`, like
/// Django's `MONTHS_3`. Used by `dateformat`'s `b` code and by
/// date-archive URLs.
#[must_use]
pub fn month_abbr(month: u32) -> &'static str {
    match month {
        1 => "jan",
        2 => "feb",
        3 => "mar",
        4 => "apr",
        5 => "may",
        6 => "jun",
        7 => "jul",
        8 => "aug",
        9 => "sep",
        10 => "oct",
        11 => "nov",
        12 => "dec",
        _ => "",
    }
}

/// Associated Press style month name, like Django's `MONTHS_AP`:
/// `Jan.`, `Feb.`, `March`, `April`, `May`, `June`, `July`, `Aug.`,
/// `Sept.`, `Oct.`, `Nov.`, `Dec.`.
#[must_use]
pub fn month_ap(month: u32) -> &'static str {
    match month {
        1 => "Jan.",
        2 => "Feb.",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "Aug.",
        9 => "Sept.",
        10 => "Oct.",
        11 => "Nov.",
        12 => "Dec.",
        _ => "",
    }
}

/// Full weekday name, like Django's `WEEKDAYS`. It takes a
/// `chrono::Weekday`, not a number, so there is no confusion about
/// where the week starts or whether the index is 0- or 1-based.
#[must_use]
pub fn weekday_full(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// Three-letter weekday name, `"Mon"` to `"Sun"`, like Django's
/// `WEEKDAYS_ABBR`.
#[must_use]
pub fn weekday_abbr(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_full_full_range() {
        let expected = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        for (i, name) in expected.iter().enumerate() {
            assert_eq!(month_full(i as u32 + 1), *name);
        }
    }

    #[test]
    fn month_abbr_lowercase_ascii() {
        let expected = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        for (i, name) in expected.iter().enumerate() {
            assert_eq!(month_abbr(i as u32 + 1), *name);
            assert!(name.chars().all(|c| c.is_ascii_lowercase()));
        }
    }

    #[test]
    fn month_ap_includes_periods_only_for_abbreviated() {
        // Short forms end with a period.
        assert!(month_ap(1).ends_with('.'));
        assert!(month_ap(2).ends_with('.'));
        assert!(month_ap(8).ends_with('.'));
        assert!(month_ap(9).ends_with('.'));
        assert!(month_ap(10).ends_with('.'));
        assert!(month_ap(11).ends_with('.'));
        assert!(month_ap(12).ends_with('.'));
        // March to July are spelled out.
        for m in 3..=7 {
            assert!(!month_ap(m).ends_with('.'));
        }
    }

    #[test]
    fn month_ap_exact_strings() {
        assert_eq!(month_ap(1), "Jan.");
        assert_eq!(month_ap(2), "Feb.");
        assert_eq!(month_ap(9), "Sept.");
        assert_eq!(month_ap(10), "Oct.");
    }

    #[test]
    fn month_out_of_range_returns_empty() {
        assert_eq!(month_full(0), "");
        assert_eq!(month_full(13), "");
        assert_eq!(month_abbr(0), "");
        assert_eq!(month_abbr(13), "");
        assert_eq!(month_ap(0), "");
        assert_eq!(month_ap(13), "");
    }

    #[test]
    fn weekday_full_all_seven() {
        assert_eq!(weekday_full(Weekday::Mon), "Monday");
        assert_eq!(weekday_full(Weekday::Tue), "Tuesday");
        assert_eq!(weekday_full(Weekday::Wed), "Wednesday");
        assert_eq!(weekday_full(Weekday::Thu), "Thursday");
        assert_eq!(weekday_full(Weekday::Fri), "Friday");
        assert_eq!(weekday_full(Weekday::Sat), "Saturday");
        assert_eq!(weekday_full(Weekday::Sun), "Sunday");
    }

    #[test]
    fn weekday_abbr_all_seven_three_chars() {
        for day in [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ] {
            assert_eq!(weekday_abbr(day).len(), 3);
        }
    }
}
