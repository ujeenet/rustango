//! Human-readable number, size and time filters for Tera.
//!
//! * Numbers: `intcomma`, `intword`, `apnumber`, `ordinal`,
//!   `format_number`, `format_currency`.
//! * Byte sizes: `naturalsize` (1024-base), `naturalsize_si`
//!   (1000-base).
//! * Time: `naturaltime`, `naturaltime_short`, `naturalday`,
//!   `timesince`, `timeuntil`, `format_duration_long`,
//!   `format_duration_short`.
//!
//! Call [`register_filters`] on a Tera instance to add them all:
//!
//! ```ignore
//! let mut tera = tera::Tera::default();
//! rustango::humanize::register_filters(&mut tera);
//! // now {{ count | intcomma }} renders "1,234,567"
//! ```
//!
//! Output is en-US. `format_number` and `format_currency` take a `locale`
//! argument; every other filter writes English words.
//!
//! [`register_filters`]: crate::humanize::register_filters

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};
use tera::{to_value, Tera, Value};

/// Add every humanize filter to `tera`. Call it at app setup, right
/// after you build the `Tera` instance.
pub fn register_filters(tera: &mut Tera) {
    tera.register_filter("intcomma", intcomma_filter);
    tera.register_filter("intword", intword_filter);
    tera.register_filter("naturalsize", naturalsize_filter);
    tera.register_filter("naturalsize_si", naturalsize_si_filter);
    tera.register_filter("ordinal", ordinal_filter);
    tera.register_filter("apnumber", apnumber_filter);
    tera.register_filter("naturaltime", naturaltime_filter);
    tera.register_filter("naturaltime_short", naturaltime_short_filter);
    tera.register_filter("naturalday", naturalday_filter);
    tera.register_filter("timesince", timesince);
    tera.register_filter("timeuntil", timeuntil);
    tera.register_filter("format_number", format_number_filter);
    tera.register_filter("format_currency", format_currency_filter);
    tera.register_filter("format_duration_long", format_duration_long_filter);
    tera.register_filter("format_duration_short", format_duration_short_filter);
}

// ------------------------------------------------------------------ intcomma

/// Put thousands separators into an integer. For floats use
/// [`intcomma_f64`].
///
/// ```
/// use rustango::humanize::intcomma;
/// assert_eq!(intcomma(4500), "4,500");
/// assert_eq!(intcomma(1_234_567), "1,234,567");
/// assert_eq!(intcomma(-1_000), "-1,000");
/// assert_eq!(intcomma(0), "0");
/// ```
#[must_use]
pub fn intcomma(n: i64) -> String {
    format_with_commas_i64(n)
}

/// [`intcomma`] for `f64`: separators go in the whole-number part,
/// the fraction is left alone.
///
/// ```
/// use rustango::humanize::intcomma_f64;
/// assert_eq!(intcomma_f64(1234567.89), "1,234,567.89");
/// assert_eq!(intcomma_f64(0.5), "0.5");
/// assert_eq!(intcomma_f64(-1000.25), "-1,000.25");
/// ```
#[must_use]
pub fn intcomma_f64(f: f64) -> String {
    let s = format!("{f}");
    let (sign, body) = if let Some(rest) = s.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", s.as_str())
    };
    let formatted = if let Some((int_part, frac_part)) = body.split_once('.') {
        let int_with_commas = comma_separate_digits(int_part);
        format!("{int_with_commas}.{frac_part}")
    } else {
        comma_separate_digits(body)
    };
    format!("{sign}{formatted}")
}

fn intcomma_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    if let Some(n) = value.as_i64() {
        return Ok(to_value(intcomma(n))?);
    }
    if let Some(n) = value.as_u64() {
        return Ok(to_value(format_with_commas_u64(n))?);
    }
    if let Some(f) = value.as_f64() {
        return Ok(to_value(intcomma_f64(f))?);
    }
    Ok(value.clone())
}

fn format_with_commas_i64(n: i64) -> String {
    let s = n.abs().to_string();
    let sep = comma_separate_digits(&s);
    if n < 0 {
        format!("-{sep}")
    } else {
        sep
    }
}

fn format_with_commas_u64(n: u64) -> String {
    comma_separate_digits(&n.to_string())
}

/// Put a comma every three digits, counting from the right.
fn comma_separate_digits(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ------------------------------------------------------------------ format_number / format_currency
//
// Locale-aware number and currency output from a small built-in
// table. Full CLDR data would add a heavy dependency for little
// gain here. An unknown locale falls back to en-US and logs a
// warning, so a misspelled code shows up in the logs.

/// Decimal point and group separator for one locale.
#[derive(Debug, Clone, Copy)]
struct NumberFmt {
    decimal: char,
    group: char,
}

fn locale_number_fmt(locale: &str) -> NumberFmt {
    // Lowercase and keep the base language only: "en-US" is "en".
    let base = locale.to_ascii_lowercase();
    let base = base.split('-').next().unwrap_or(&base);
    match base {
        // Dot decimal, comma groups.
        "en" | "ja" | "zh" | "ko" | "th" => NumberFmt {
            decimal: '.',
            group: ',',
        },
        // Comma decimal, dot groups.
        "de" | "es" | "it" | "nl" | "pt" | "el" | "pl" | "tr" | "da" | "fi" | "sv" | "no"
        | "nb" | "nn" => NumberFmt {
            decimal: ',',
            group: '.',
        },
        // Comma decimal, space groups.
        "fr" | "ru" | "cs" | "sk" | "bg" | "uk" | "hu" => NumberFmt {
            decimal: ',',
            group: ' ',
        },
        _ => {
            tracing::warn!(
                target: "rustango::humanize",
                locale = %locale,
                "format_number: unknown locale, falling back to en-US convention"
            );
            NumberFmt {
                decimal: '.',
                group: ',',
            }
        }
    }
}

/// Join the digits before and after the decimal using `fmt`. Pass
/// `""` for `frac_part` when there is none. The caller keeps the
/// minus sign.
fn apply_number_fmt(integer_part: &str, frac_part: &str, fmt: NumberFmt) -> String {
    let bytes = integer_part.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3 + frac_part.len() + 1);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(fmt.group);
        }
        out.push(*b as char);
    }
    if !frac_part.is_empty() {
        out.push(fmt.decimal);
        out.push_str(frac_part);
    }
    out
}

/// Format a number with the decimal point and thousands separator of
/// a locale. Currency symbols live in [`format_currency`].
///
/// Only the base language counts, so `"en-US"` acts as `"en"`. An
/// unknown locale falls back to `"en"` and logs a warning.
///
/// `decimals` of `None` keeps the input's own precision. `Some(n)`
/// pads or cuts the fraction to exactly `n` digits.
///
/// Separators by language:
/// * en, ja, zh, ko, th: `.` decimal, `,` groups
/// * de, es, it, nl, pt, pl, sv, nb, nn, da, fi, el, tr: `,` decimal,
///   `.` groups
/// * fr, ru, cs, sk, bg, uk, hu: `,` decimal, space groups
///
/// As a Tera filter it takes the same `locale` and `decimals`
/// arguments, and passes non-numeric input through unchanged:
///
/// ```jinja
/// {{ 1234567.89 | format_number(locale="de") }}            {# → 1.234.567,89 #}
/// {{ 1234.5    | format_number(locale="en", decimals=2) }} {# → 1,234.50 #}
/// ```
///
/// ```
/// use rustango::humanize::format_number;
/// assert_eq!(format_number(1234567.89, "en", None),    "1,234,567.89");
/// assert_eq!(format_number(1234567.89, "de", None),    "1.234.567,89");
/// assert_eq!(format_number(1234567.89, "fr", None),    "1 234 567,89");
/// assert_eq!(format_number(1234.5,     "en", Some(2)), "1,234.50");
/// assert_eq!(format_number(0.0,        "en", Some(0)), "0");
/// assert_eq!(format_number(-1234.5,    "en", None),    "-1,234.5");
/// ```
#[must_use]
pub fn format_number(value: f64, locale: &str, decimals: Option<usize>) -> String {
    let fmt = locale_number_fmt(locale);
    let s = match decimals {
        Some(d) => format!("{value:.*}", d),
        None => format!("{value}"),
    };
    let negative = s.starts_with('-');
    let body = if negative { &s[1..] } else { &s };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let formatted = apply_number_fmt(int_part, frac_part, fmt);
    if negative {
        format!("-{formatted}")
    } else {
        formatted
    }
}

fn format_number_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let locale = args.get("locale").and_then(Value::as_str).unwrap_or("en");
    let decimals = args
        .get("decimals")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    // Integers take their own path so they gain no trailing zeros
    // unless `decimals` asked for them.
    if let Some(n) = value.as_i64() {
        let fmt = locale_number_fmt(locale);
        let abs = n.unsigned_abs().to_string();
        let frac = match decimals {
            Some(d) => "0".repeat(d),
            None => String::new(),
        };
        let body = apply_number_fmt(&abs, &frac, fmt);
        return Ok(to_value(if n < 0 { format!("-{body}") } else { body })?);
    }
    if let Some(n) = value.as_u64() {
        let fmt = locale_number_fmt(locale);
        let frac = match decimals {
            Some(d) => "0".repeat(d),
            None => String::new(),
        };
        return Ok(to_value(apply_number_fmt(&n.to_string(), &frac, fmt))?);
    }
    if let Some(f) = value.as_f64() {
        return Ok(to_value(format_number(f, locale, decimals))?);
    }
    Ok(value.clone())
}

/// How to show one currency.
#[derive(Debug, Clone, Copy)]
struct CurrencyFmt {
    /// The symbol, such as "$", "€" or "₽".
    symbol: &'static str,
    /// `true` puts the symbol first (`$1,234.56`), `false` last
    /// (`1.234,56 €`).
    prefix: bool,
    /// Decimal places. Most use 2; JPY, KRW and CLP use 0.
    decimals: u32,
}

fn currency_fmt(code: &str) -> CurrencyFmt {
    let code = code.to_ascii_uppercase();
    match code.as_str() {
        "USD" | "CAD" | "AUD" | "NZD" | "HKD" | "SGD" | "MXN" => CurrencyFmt {
            symbol: "$",
            prefix: true,
            decimals: 2,
        },
        "EUR" => CurrencyFmt {
            symbol: "€",
            prefix: true,
            decimals: 2,
        },
        "GBP" => CurrencyFmt {
            symbol: "£",
            prefix: true,
            decimals: 2,
        },
        "JPY" | "KRW" | "CLP" => CurrencyFmt {
            // No sub-unit in circulation.
            symbol: "¥",
            prefix: true,
            decimals: 0,
        },
        "CNY" => CurrencyFmt {
            symbol: "¥",
            prefix: true,
            decimals: 2,
        },
        "RUB" => CurrencyFmt {
            symbol: "₽",
            prefix: false,
            decimals: 2,
        },
        "INR" => CurrencyFmt {
            symbol: "₹",
            prefix: true,
            decimals: 2,
        },
        "BRL" => CurrencyFmt {
            symbol: "R$",
            prefix: true,
            decimals: 2,
        },
        "CHF" => CurrencyFmt {
            symbol: "CHF",
            prefix: true,
            decimals: 2,
        },
        // Unknown code: show the code itself, so the output still
        // says something.
        _ => {
            tracing::warn!(
                target: "rustango::humanize",
                currency = %code,
                "format_currency: unknown ISO 4217 code, using raw code as symbol"
            );
            CurrencyFmt {
                symbol: Box::leak(code.into_boxed_str()),
                prefix: true,
                decimals: 2,
            }
        }
    }
}

/// Format an amount of money: [`format_number`] plus the currency's
/// symbol, in the place that currency uses.
///
/// `currency` is an ISO 4217 code. It sets the symbol and the number
/// of decimals: 2 for most, 0 for JPY, KRW and CLP. An unknown code
/// is used as its own symbol and logs a warning. `locale` only sets
/// the separators.
///
/// The euro is the one symbol whose place depends on the locale: fr,
/// it, es, pt and nl put `€` after the amount, everyone else before.
///
/// As a Tera filter it takes the same `currency` (default `"USD"`)
/// and `locale` (default `"en"`) arguments, and passes non-numeric
/// input through unchanged.
///
/// ```
/// use rustango::humanize::format_currency;
/// assert_eq!(format_currency(1234.56, "USD", "en"), "$1,234.56");
/// assert_eq!(format_currency(1234.56, "EUR", "de"), "€1.234,56");
/// assert_eq!(format_currency(1234.56, "EUR", "fr"), "1 234,56 €");
/// assert_eq!(format_currency(1234.0,  "JPY", "ja"), "¥1,234");   // 0 decimals
/// assert_eq!(format_currency(-50.0,   "USD", "en"), "-$50.00");  // sign before symbol
/// ```
#[must_use]
pub fn format_currency(amount: f64, currency: &str, locale: &str) -> String {
    let cur = currency_fmt(currency);
    let fmt = locale_number_fmt(locale);

    let body_str = format!("{amount:.*}", cur.decimals as usize);
    let negative = body_str.starts_with('-');
    let unsigned = if negative { &body_str[1..] } else { &body_str };
    let (int_part, frac_part) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let formatted = apply_number_fmt(int_part, frac_part, fmt);
    let sign = if negative { "-" } else { "" };

    let base = locale.to_ascii_lowercase();
    let base = base.split('-').next().unwrap_or(&base);
    let euro_suffix_locale = matches!(base, "fr" | "it" | "es" | "pt" | "nl");
    let prefix_after_override = if currency.eq_ignore_ascii_case("EUR") && euro_suffix_locale {
        false
    } else {
        cur.prefix
    };

    if prefix_after_override {
        format!("{sign}{symbol}{formatted}", symbol = cur.symbol)
    } else {
        format!("{sign}{formatted} {symbol}", symbol = cur.symbol)
    }
}

fn format_currency_filter(value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let currency = args
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or("USD");
    let locale = args.get("locale").and_then(Value::as_str).unwrap_or("en");

    let amount = match value.as_f64() {
        Some(f) => f,
        None => match value.as_i64() {
            Some(i) => i as f64,
            None => match value.as_u64() {
                Some(u) => u as f64,
                None => return Ok(value.clone()),
            },
        },
    };

    Ok(to_value(format_currency(amount, currency, locale))?)
}

// ------------------------------------------------------------------ intword

/// Write a large number as a word: `1_200_000` becomes
/// `"1.2 million"`. Below one million the integer comes back plain.
///
/// The scales run from million up to decillion (`1e6` to `1e33`).
/// Anything bigger stays on the decillion scale.
///
/// ```
/// use rustango::humanize::intword;
/// assert_eq!(intword(1_200_000.0), "1.2 million");
/// assert_eq!(intword(1_000_000_000.0), "1.0 billion");
/// assert_eq!(intword(999_999.0), "999999");
/// assert_eq!(intword(-1_500_000.0), "-1.5 million");
/// ```
pub fn intword(n: f64) -> String {
    if n.abs() < 1_000_000.0 {
        // Under 1M the integer comes back as is.
        return format!("{}", n.trunc() as i64);
    }
    let scales: &[(f64, &str)] = &[
        (1e6, "million"),
        (1e9, "billion"),
        (1e12, "trillion"),
        (1e15, "quadrillion"),
        (1e18, "quintillion"),
        (1e21, "sextillion"),
        (1e24, "septillion"),
        (1e27, "octillion"),
        (1e30, "nonillion"),
        (1e33, "decillion"),
    ];
    let mut chosen = scales[0];
    for &(s, name) in scales {
        if n.abs() >= s {
            chosen = (s, name);
        } else {
            break;
        }
    }
    let scaled = n / chosen.0;
    format!("{:.1} {}", scaled, chosen.1)
}

fn intword_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v as f64,
        None => match value.as_f64() {
            Some(v) => v,
            None => return Ok(value.clone()),
        },
    };
    if n.abs() < 1_000_000.0 {
        if let Some(i) = value.as_i64() {
            return Ok(to_value(i.to_string())?);
        }
        return Ok(value.clone());
    }
    Ok(to_value(intword(n))?)
}

// ------------------------------------------------------------------ naturalsize

/// A byte count people can read, on the 1024 scale. Under 1024 you
/// get `"N bytes"`, or `"1 byte"` for exactly one.
///
/// Units: bytes, KB, MB, GB, TB, PB, EB, ZB, YB.
///
/// ```
/// use rustango::humanize::naturalsize;
/// assert_eq!(naturalsize(1024.0), "1.0 KB");
/// assert_eq!(naturalsize(1_572_864.0), "1.5 MB");
/// assert_eq!(naturalsize(1.0), "1 byte");
/// assert_eq!(naturalsize(0.0), "0 bytes");
/// ```
pub fn naturalsize(n: f64) -> String {
    let units = ["bytes", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    if n < 1024.0 {
        if (n - 1.0).abs() < f64::EPSILON {
            return "1 byte".to_owned();
        }
        return format!("{} bytes", n as u64);
    }
    let mut scale = 0_usize;
    let mut scaled = n;
    while scaled >= 1024.0 && scale < units.len() - 1 {
        scaled /= 1024.0;
        scale += 1;
    }
    format!("{:.1} {}", scaled, units[scale])
}

/// [`naturalsize`] on the 1000 scale instead of 1024. Use it when
/// the number should match what a disk vendor prints on the box, or
/// a quota expressed in decimal units.
///
/// Units: bytes, kB, MB, GB, TB, PB, EB, ZB, YB. The `k` is
/// lowercase, per SI.
///
/// ```
/// use rustango::humanize::naturalsize_si;
/// assert_eq!(naturalsize_si(1000.0), "1.0 kB");
/// assert_eq!(naturalsize_si(1_500_000.0), "1.5 MB");
/// assert_eq!(naturalsize_si(1.0), "1 byte");
/// assert_eq!(naturalsize_si(0.0), "0 bytes");
/// // 1024 in 1000-base sits at 1.0 kB rounded.
/// assert_eq!(naturalsize_si(1024.0), "1.0 kB");
/// ```
pub fn naturalsize_si(n: f64) -> String {
    // Lowercase `k` for kilo, per ISO 80000-13.
    let units = ["bytes", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    if n < 1000.0 {
        if (n - 1.0).abs() < f64::EPSILON {
            return "1 byte".to_owned();
        }
        return format!("{} bytes", n as u64);
    }
    let mut scale = 0_usize;
    let mut scaled = n;
    while scaled >= 1000.0 && scale < units.len() - 1 {
        scaled /= 1000.0;
        scale += 1;
    }
    format!("{:.1} {}", scaled, units[scale])
}

fn naturalsize_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_u64() {
        Some(v) => v as f64,
        None => match value.as_i64() {
            Some(v) if v >= 0 => v as f64,
            _ => match value.as_f64() {
                Some(v) => v,
                None => return Ok(value.clone()),
            },
        },
    };
    Ok(to_value(naturalsize(n))?)
}

fn naturalsize_si_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_u64() {
        Some(v) => v as f64,
        None => match value.as_i64() {
            Some(v) if v >= 0 => v as f64,
            _ => match value.as_f64() {
                Some(v) => v,
                None => return Ok(value.clone()),
            },
        },
    };
    Ok(to_value(naturalsize_si(n))?)
}

// ------------------------------------------------------------------ ordinal

/// Add the English ordinal suffix: `1` becomes `"1st"`, `2` becomes
/// `"2nd"`. 11, 12 and 13 always take "th"; every other number
/// follows its last digit. A negative number takes the same suffix
/// as its absolute value.
///
/// ```
/// use rustango::humanize::ordinal;
/// assert_eq!(ordinal(1), "1st");
/// assert_eq!(ordinal(2), "2nd");
/// assert_eq!(ordinal(11), "11th");
/// assert_eq!(ordinal(21), "21st");
/// assert_eq!(ordinal(-3), "-3rd");
/// ```
#[must_use]
pub fn ordinal(n: i64) -> String {
    format!("{n}{}", ordinal_suffix(n.unsigned_abs()))
}

fn ordinal_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v,
        None => return Ok(value.clone()),
    };
    Ok(to_value(ordinal(n))?)
}

fn ordinal_suffix(n: u64) -> &'static str {
    // The teens take "th", so check them before the last digit.
    let last_two = n % 100;
    if (11..=13).contains(&last_two) {
        return "th";
    }
    match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

// ------------------------------------------------------------------ apnumber

/// Spell out 1 to 9 as `"one"` to `"nine"`, following AP style. Any
/// other value comes back as its digits.
///
/// ```
/// use rustango::humanize::apnumber;
/// assert_eq!(apnumber(1), "one");
/// assert_eq!(apnumber(9), "nine");
/// assert_eq!(apnumber(10), "10");
/// assert_eq!(apnumber(0), "0");
/// assert_eq!(apnumber(-3), "-3");
/// ```
#[must_use]
pub fn apnumber(n: i64) -> String {
    match n {
        1 => "one".to_owned(),
        2 => "two".to_owned(),
        3 => "three".to_owned(),
        4 => "four".to_owned(),
        5 => "five".to_owned(),
        6 => "six".to_owned(),
        7 => "seven".to_owned(),
        8 => "eight".to_owned(),
        9 => "nine".to_owned(),
        _ => n.to_string(),
    }
}

fn apnumber_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let n = match value.as_i64() {
        Some(v) => v,
        None => return Ok(value.clone()),
    };
    Ok(to_value(apnumber(n))?)
}

// ------------------------------------------------------------------ naturaltime

/// `naturaltime` — a time relative to now, such as "3 minutes ago"
/// or "in 5 hours". Takes an RFC 3339 string or anything serde can
/// read as `DateTime<Utc>`.
///
/// Under 30 seconds reads "now". Then the unit grows: seconds,
/// minutes, hours, days, months, years.
fn naturaltime_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    Ok(to_value(natural_time_string(now, dt))?)
}

fn naturaltime_short_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    Ok(to_value(naturaltime_short(Utc::now(), dt))?)
}

fn parse_datetime(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = value.as_str() {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    serde_json::from_value(value.clone()).ok()
}

/// A time relative to `now`: `"3 minutes ago"`, `"in 5 hours"`, or
/// `"now"` within 30 seconds either way.
///
/// It reports one unit only, the largest that fits. For
/// "4 days, 6 hours" use [`crate::timesince::timesince`].
///
/// ```
/// use chrono::{Duration, TimeZone, Utc};
/// use rustango::humanize::naturaltime;
///
/// let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
/// let past = now - Duration::minutes(3);
/// assert_eq!(naturaltime(now, past), "3 minutes ago");
///
/// let future = now + Duration::hours(5);
/// assert_eq!(naturaltime(now, future), "in 5 hours");
///
/// assert_eq!(naturaltime(now, now), "now");
/// ```
#[must_use]
pub fn naturaltime(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    natural_time_string(now, then)
}

/// [`naturaltime`] in a short form for tight spaces: `"45s ago"`,
/// `"3m ago"`, `"4h ago"`, `"2d ago"`, `"3mo ago"`, `"2y ago"`, or
/// `"now"`. A future time reads `"in 10m"`.
///
/// Months count as 30 days and years as 365. The cut-off points are
/// the same as [`naturaltime`].
///
/// ```ignore
/// use chrono::{Duration, TimeZone, Utc};
/// use rustango::humanize::naturaltime_short;
///
/// let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
/// assert_eq!(naturaltime_short(now, now), "now");
/// assert_eq!(naturaltime_short(now, now - Duration::seconds(45)), "45s ago");
/// assert_eq!(naturaltime_short(now, now - Duration::minutes(5)), "5m ago");
/// assert_eq!(naturaltime_short(now, now - Duration::hours(3)), "3h ago");
/// assert_eq!(naturaltime_short(now, now + Duration::minutes(10)), "in 10m");
/// ```
#[must_use]
pub fn naturaltime_short(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let abs = delta.num_seconds().abs();
    if abs < 30 {
        return "now".to_owned();
    }
    let past = delta.num_seconds() >= 0;
    let (n, unit) = if abs < 60 {
        (abs, "s")
    } else if abs < 3600 {
        (abs / 60, "m")
    } else if abs < 86_400 {
        (abs / 3600, "h")
    } else if abs < 2_592_000 {
        (abs / 86_400, "d")
    } else if abs < 31_536_000 {
        (abs / 2_592_000, "mo")
    } else {
        (abs / 31_536_000, "y")
    };
    if past {
        format!("{n}{unit} ago")
    } else {
        format!("in {n}{unit}")
    }
}

/// Name a day relative to today: `"today"`, `"yesterday"`,
/// `"tomorrow"`, or a date like `"Apr 27"`.
///
/// ```
/// use chrono::{Duration, TimeZone, Utc};
/// use rustango::humanize::naturalday;
///
/// let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
/// assert_eq!(naturalday(now, now), "today");
/// assert_eq!(naturalday(now, now - Duration::days(1)), "yesterday");
/// assert_eq!(naturalday(now, now + Duration::days(1)), "tomorrow");
/// assert_eq!(naturalday(now, now - Duration::days(45)),
///            "Apr 21");
/// ```
#[must_use]
pub fn naturalday(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    natural_day_string(now, then)
}

fn natural_time_string(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let abs = delta.num_seconds().abs();
    let suffix = if delta.num_seconds() >= 0 {
        "ago"
    } else {
        "from now"
    };

    if abs < 30 {
        return "now".to_owned();
    }
    if abs < 60 {
        return format_unit(abs, "second", suffix);
    }
    let minutes = abs / 60;
    if minutes < 60 {
        return format_unit(minutes, "minute", suffix);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format_unit(hours, "hour", suffix);
    }
    let days = hours / 24;
    if days < 30 {
        return format_unit(days, "day", suffix);
    }
    let months = days / 30;
    if months < 12 {
        return format_unit(months, "month", suffix);
    }
    let years = days / 365;
    format_unit(years, "year", suffix)
}

fn format_unit(n: i64, unit: &str, suffix: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    if suffix == "ago" {
        format!("{n} {unit}{plural} ago")
    } else {
        format!("in {n} {unit}{plural}")
    }
}

/// Tera filter for `format_duration_long`. Takes a number of
/// seconds, or a string holding seconds or an ISO-8601 duration
/// like `"PT1H2M5S"`. Anything else passes through unchanged.
fn format_duration_long_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(d) = duration_from_value(value) else {
        return Ok(value.clone());
    };
    Ok(to_value(format_duration_long(d))?)
}

/// Tera filter for `format_duration_short`. Same input acceptance
/// as the long form.
fn format_duration_short_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let Some(d) = duration_from_value(value) else {
        return Ok(value.clone());
    };
    Ok(to_value(format_duration_short(d))?)
}

fn duration_from_value(value: &Value) -> Option<chrono::Duration> {
    match value {
        Value::Number(n) => n.as_i64().map(chrono::Duration::seconds),
        Value::String(s) => {
            if let Ok(secs) = s.parse::<i64>() {
                return Some(chrono::Duration::seconds(secs));
            }
            // `parse_duration` gives a `std::time::Duration`.
            crate::dateparse::parse_duration(s).and_then(|d| chrono::Duration::from_std(d).ok())
        }
        _ => None,
    }
}

// ------------------------------------------------------------------ format_duration_long / format_duration_short

/// Spell out a [`chrono::Duration`], as in
/// `"2 hours, 5 minutes, 3 seconds"`. Empty parts are left out and
/// a zero duration reads `"0 seconds"`. A negative duration gets a
/// leading minus sign.
///
/// Use it for a length of time on its own, such as elapsed work or
/// a task's runtime. For a time relative to now, use `timesince`.
///
/// ```ignore
/// use chrono::Duration;
/// use rustango::humanize::format_duration_long;
///
/// assert_eq!(format_duration_long(Duration::seconds(0)), "0 seconds");
/// assert_eq!(format_duration_long(Duration::seconds(45)), "45 seconds");
/// assert_eq!(format_duration_long(Duration::seconds(125)), "2 minutes, 5 seconds");
/// assert_eq!(format_duration_long(Duration::seconds(3725)), "1 hour, 2 minutes, 5 seconds");
/// assert_eq!(format_duration_long(Duration::days(2) + Duration::hours(3)), "2 days, 3 hours");
/// ```
#[must_use]
pub fn format_duration_long(d: chrono::Duration) -> String {
    let sign = if d.num_seconds() < 0 { "-" } else { "" };
    let total = d.num_seconds().unsigned_abs();
    if total == 0 {
        return "0 seconds".to_owned();
    }
    let days = total / 86_400;
    let hours = (total % 86_400) / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    let mut parts: Vec<String> = Vec::with_capacity(4);
    let push = |parts: &mut Vec<String>, n: u64, unit: &str| {
        if n > 0 {
            let plural = if n == 1 { "" } else { "s" };
            parts.push(format!("{n} {unit}{plural}"));
        }
    };
    push(&mut parts, days, "day");
    push(&mut parts, hours, "hour");
    push(&mut parts, mins, "minute");
    push(&mut parts, secs, "second");
    format!("{sign}{}", parts.join(", "))
}

/// [`format_duration_long`] in compact form, as in `"2h5m3s"`. Empty
/// parts are left out, zero reads `"0s"`, and a negative duration
/// gets a leading minus sign. Use it where the long form is too
/// wide, such as a dashboard or a table cell.
///
/// ```ignore
/// use chrono::Duration;
/// use rustango::humanize::format_duration_short;
///
/// assert_eq!(format_duration_short(Duration::seconds(0)), "0s");
/// assert_eq!(format_duration_short(Duration::seconds(45)), "45s");
/// assert_eq!(format_duration_short(Duration::seconds(125)), "2m5s");
/// assert_eq!(format_duration_short(Duration::seconds(3725)), "1h2m5s");
/// assert_eq!(format_duration_short(Duration::days(2) + Duration::hours(3)), "2d3h");
/// ```
#[must_use]
pub fn format_duration_short(d: chrono::Duration) -> String {
    let sign = if d.num_seconds() < 0 { "-" } else { "" };
    let total = d.num_seconds().unsigned_abs();
    if total == 0 {
        return "0s".to_owned();
    }
    let days = total / 86_400;
    let hours = (total % 86_400) / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    let mut out = String::with_capacity(16);
    out.push_str(sign);
    if days > 0 {
        out.push_str(&format!("{days}d"));
    }
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if mins > 0 {
        out.push_str(&format!("{mins}m"));
    }
    if secs > 0 {
        out.push_str(&format!("{secs}s"));
    }
    out
}

// ------------------------------------------------------------------ naturalday

fn naturalday_filter(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    Ok(to_value(natural_day_string(Utc::now(), dt))?)
}

fn natural_day_string(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let today = now.date_naive();
    let other = then.date_naive();
    let diff = (other - today).num_days();
    match diff {
        0 => "today".to_owned(),
        -1 => "yesterday".to_owned(),
        1 => "tomorrow".to_owned(),
        _ => {
            let month = match other.month() {
                1 => "Jan",
                2 => "Feb",
                3 => "Mar",
                4 => "Apr",
                5 => "May",
                6 => "Jun",
                7 => "Jul",
                8 => "Aug",
                9 => "Sep",
                10 => "Oct",
                11 => "Nov",
                12 => "Dec",
                _ => unreachable!(),
            };
            format!("{month} {:02}", other.day())
        }
    }
}

// ------------------------------------------------------------------ timesince / timeuntil

/// `"N units"` with no `"ago"` or `"in"` around it, for
/// [`timesince`] and [`timeuntil`].
///
/// A delta of zero or less reads `"0 minutes"`: no time has passed
/// yet.
fn magnitude_string(seconds: i64) -> String {
    if seconds <= 0 {
        return "0 minutes".to_owned();
    }
    if seconds < 60 {
        return format_magnitude(seconds, "second");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format_magnitude(minutes, "minute");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format_magnitude(hours, "hour");
    }
    let days = hours / 24;
    if days < 30 {
        return format_magnitude(days, "day");
    }
    let months = days / 30;
    if months < 12 {
        return format_magnitude(months, "month");
    }
    let years = days / 365;
    format_magnitude(years, "year")
}

fn format_magnitude(n: i64, unit: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural}")
}

/// `timesince` — how long ago `value` was, as `"N units"`. A future
/// time reads `"0 minutes"`; use [`timeuntil`] for those.
///
/// It reports one unit, on the same cut-offs as [`naturaltime`].
fn timesince(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    let delta = now.signed_duration_since(dt).num_seconds();
    Ok(to_value(magnitude_string(delta))?)
}

/// `timeuntil` — how long until `value`, as `"N units"`. The mirror
/// of [`timesince`]. A past time reads `"0 minutes"`.
fn timeuntil(value: &Value, _: &HashMap<String, Value>) -> tera::Result<Value> {
    let dt = match parse_datetime(value) {
        Some(d) => d,
        None => return Ok(value.clone()),
    };
    let now = Utc::now();
    let delta = dt.signed_duration_since(now).num_seconds();
    Ok(to_value(magnitude_string(delta))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn render(tera: &Tera, src: &str, ctx: tera::Context) -> String {
        let mut t = tera.clone();
        t.add_raw_template("_", src).unwrap();
        t.render("_", &ctx).unwrap()
    }

    fn setup() -> Tera {
        let mut tera = Tera::default();
        register_filters(&mut tera);
        tera
    }

    // -------- intcomma --------

    #[test]
    fn intcomma_handles_small_ints() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &450_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "450");
    }

    #[test]
    fn intcomma_inserts_separators() {
        let tera = setup();
        for (n, expected) in [
            (1_234_i64, "1,234"),
            (1_234_567, "1,234,567"),
            (1_000_000_000, "1,000,000,000"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(
                render(&tera, "{{ n | intcomma }}", ctx),
                expected,
                "for n={n}"
            );
        }
    }

    #[test]
    fn intcomma_handles_negative_ints() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &-1_234_567_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "-1,234,567");
    }

    #[test]
    fn intcomma_preserves_decimal_part() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &1_234_567.89_f64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "1,234,567.89");
    }

    // -------- intword --------

    #[test]
    fn intword_below_million_unchanged() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &999_999_i64);
        assert_eq!(render(&tera, "{{ n | intword }}", ctx), "999999");
    }

    #[test]
    fn intword_million_scale() {
        let tera = setup();
        for (n, expected) in [
            (1_200_000_i64, "1.2 million"),
            (1_000_000, "1.0 million"),
            (2_500_000_000, "2.5 billion"),
            (1_000_000_000_000_i64, "1.0 trillion"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | intword }}", ctx), expected, "n={n}");
        }
    }

    // -------- naturalsize --------

    #[test]
    fn naturalsize_byte_threshold() {
        let tera = setup();
        for (bytes, expected) in [
            (0_u64, "0 bytes"),
            (1, "1 byte"),
            (512, "512 bytes"),
            (1_023, "1023 bytes"),
            (1_024, "1.0 KB"),
            (1_536, "1.5 KB"),
            (1_572_864, "1.5 MB"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("b", &bytes);
            assert_eq!(
                render(&tera, "{{ b | naturalsize }}", ctx),
                expected,
                "bytes={bytes}"
            );
        }
    }

    // -------- naturalsize_si --------

    #[test]
    fn naturalsize_si_uses_decimal_si_units() {
        let tera = setup();
        // SI breaks at 1000 and writes "kB", not "KB".
        for (bytes, expected) in [
            (0_u64, "0 bytes"),
            (999, "999 bytes"),
            (1_000, "1.0 kB"),
            (1_500_000, "1.5 MB"),
            (2_000_000_000, "2.0 GB"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("b", &bytes);
            assert_eq!(
                render(&tera, "{{ b | naturalsize_si }}", ctx),
                expected,
                "bytes={bytes}"
            );
        }
    }

    // -------- ordinal --------

    #[test]
    fn ordinal_picks_correct_suffix() {
        let tera = setup();
        for (n, expected) in [
            (1_i64, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (10, "10th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (14, "14th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (101, "101st"),
            (111, "111th"),
            (112, "112th"),
            (113, "113th"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | ordinal }}", ctx), expected, "n={n}");
        }
    }

    // -------- apnumber --------

    #[test]
    fn apnumber_spells_one_through_nine() {
        let tera = setup();
        for (n, expected) in [
            (1_i64, "one"),
            (5, "five"),
            (9, "nine"),
            (10, "10"),
            (42, "42"),
            (0, "0"),
        ] {
            let mut ctx = tera::Context::new();
            ctx.insert("n", &n);
            assert_eq!(render(&tera, "{{ n | apnumber }}", ctx), expected, "n={n}");
        }
    }

    // -------- naturaltime --------

    #[test]
    fn naturaltime_buckets_correctly() {
        let now = Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        for (offset_secs, expected) in [
            (5, "now"),
            (45, "45 seconds ago"),
            (-45, "in 45 seconds"),
            (60, "1 minute ago"),
            (120, "2 minutes ago"),
            (3600, "1 hour ago"),
            (7200, "2 hours ago"),
            (86_400, "1 day ago"),
            (86_400 * 2, "2 days ago"),
            (86_400 * 31, "1 month ago"),
            (86_400 * 400, "1 year ago"),
            (-3600, "in 1 hour"),
        ] {
            let then = now - Duration::seconds(offset_secs);
            assert_eq!(
                natural_time_string(now, then),
                expected,
                "offset={offset_secs}"
            );
        }
    }

    // -------- naturalday --------

    #[test]
    fn naturalday_keywords() {
        let now = Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        let today = now;
        let yesterday = now - Duration::days(1);
        let tomorrow = now + Duration::days(1);
        let week_ago = now - Duration::days(7);
        assert_eq!(natural_day_string(now, today), "today");
        assert_eq!(natural_day_string(now, yesterday), "yesterday");
        assert_eq!(natural_day_string(now, tomorrow), "tomorrow");
        // 7 days before 2026-05-16 is 2026-05-09.
        assert_eq!(natural_day_string(now, week_ago), "May 09");
    }

    // -------- register --------

    #[test]
    fn register_filters_makes_them_callable_via_tera() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &1_000_000_i64);
        assert_eq!(render(&tera, "{{ n | intcomma }}", ctx), "1,000,000");
    }

    // -------- timesince / timeuntil --------

    #[test]
    fn magnitude_string_buckets_match_naturaltime() {
        assert_eq!(magnitude_string(0), "0 minutes");
        assert_eq!(magnitude_string(-5), "0 minutes");
        assert_eq!(magnitude_string(1), "1 second");
        assert_eq!(magnitude_string(45), "45 seconds");
        assert_eq!(magnitude_string(60), "1 minute");
        assert_eq!(magnitude_string(120), "2 minutes");
        assert_eq!(magnitude_string(60 * 60), "1 hour");
        assert_eq!(magnitude_string(60 * 60 * 5), "5 hours");
        assert_eq!(magnitude_string(60 * 60 * 24), "1 day");
        assert_eq!(magnitude_string(60 * 60 * 24 * 31), "1 month");
        assert_eq!(magnitude_string(60 * 60 * 24 * 366), "1 year");
    }

    #[test]
    fn timesince_filter_emits_magnitude_for_past() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        // Roughly two hours ago.
        let then = Utc::now() - Duration::hours(2);
        ctx.insert("then", &then.to_rfc3339());
        let out = render(&tera, "{{ then | timesince }}", ctx);
        assert_eq!(out, "2 hours");
    }

    #[test]
    fn timesince_filter_emits_zero_for_future() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        let later = Utc::now() + Duration::hours(2);
        ctx.insert("later", &later.to_rfc3339());
        let out = render(&tera, "{{ later | timesince }}", ctx);
        assert_eq!(out, "0 minutes");
    }

    #[test]
    fn timeuntil_filter_emits_magnitude_for_future() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        // A wide gap, so clock drift between insert and render
        // cannot cross a day boundary.
        let later = Utc::now() + Duration::days(3) + Duration::hours(1);
        ctx.insert("later", &later.to_rfc3339());
        let out = render(&tera, "{{ later | timeuntil }}", ctx);
        assert_eq!(out, "3 days", "got: {out}");
    }

    #[test]
    fn timeuntil_filter_emits_zero_for_past() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        let then = Utc::now() - Duration::days(3);
        ctx.insert("then", &then.to_rfc3339());
        let out = render(&tera, "{{ then | timeuntil }}", ctx);
        assert_eq!(out, "0 minutes");
    }

    #[test]
    fn timesince_pluralizes_correctly() {
        assert_eq!(magnitude_string(1), "1 second");
        assert_eq!(magnitude_string(2), "2 seconds");
        assert_eq!(magnitude_string(60), "1 minute");
        assert_eq!(magnitude_string(120), "2 minutes");
    }

    // -------- format_number + format_currency --------

    fn render_filter(template: &str, ctx: tera::Context) -> String {
        let mut tera = Tera::default();
        tera.add_raw_template("t", template).unwrap();
        register_filters(&mut tera);
        tera.render("t", &ctx).unwrap()
    }

    #[test]
    fn format_number_en_uses_comma_grouping_dot_decimal() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234567.89_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="en") }}"#, ctx),
            "1,234,567.89"
        );
    }

    #[test]
    fn format_number_de_uses_dot_grouping_comma_decimal() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234567.89_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="de") }}"#, ctx),
            "1.234.567,89"
        );
    }

    #[test]
    fn format_number_fr_uses_space_grouping_comma_decimal() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234567.89_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="fr") }}"#, ctx),
            "1 234 567,89"
        );
    }

    #[test]
    fn format_number_decimals_arg_pads_and_truncates() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        let out = render_filter(r#"{{ x | format_number(locale="en", decimals=2) }}"#, ctx);
        assert_eq!(out, "1,234.50");

        let mut ctx2 = tera::Context::new();
        ctx2.insert("x", &1234.5678_f64);
        let out2 = render_filter(r#"{{ x | format_number(locale="en", decimals=2) }}"#, ctx2);
        assert_eq!(out2, "1,234.57");
    }

    #[test]
    fn format_number_negative_carries_sign() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &-1234.5_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="en") }}"#, ctx),
            "-1,234.5"
        );
    }

    #[test]
    fn format_number_integer_input_works() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1_234_567i64);
        assert_eq!(
            render_filter(r#"{{ x | format_number(locale="en") }}"#, ctx),
            "1,234,567"
        );
    }

    #[test]
    fn format_number_unknown_locale_falls_back_to_en() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        // "xx-YZ" has no entry, so en-US is used.
        let out = render_filter(r#"{{ x | format_number(locale="xx-YZ") }}"#, ctx);
        assert_eq!(out, "1,234.5");
    }

    #[test]
    fn format_currency_usd_en_renders_dollar_prefix_2dp() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_currency(currency="USD") }}"#, ctx),
            "$1,234.50"
        );
    }

    #[test]
    fn format_currency_eur_fr_renders_symbol_suffix() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.5_f64);
        let out = render_filter(
            r#"{{ x | format_currency(currency="EUR", locale="fr") }}"#,
            ctx,
        );
        assert_eq!(out, "1 234,50 €");
    }

    #[test]
    fn format_currency_jpy_uses_zero_decimals() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.567_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_currency(currency="JPY") }}"#, ctx),
            "¥1,235"
        );
    }

    #[test]
    fn format_currency_negative_amount() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &-1234.5_f64);
        assert_eq!(
            render_filter(r#"{{ x | format_currency(currency="USD") }}"#, ctx),
            "-$1,234.50"
        );
    }

    #[test]
    fn format_currency_unknown_code_uses_code_as_symbol() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", &1234.0_f64);
        // ZZX is not a real currency code.
        let out = render_filter(r#"{{ x | format_currency(currency="ZZX") }}"#, ctx);
        assert!(out.contains("ZZX"), "got: {out}");
        assert!(out.contains("1,234.00"), "got: {out}");
    }

    // -------- the plain Rust functions --------

    #[test]
    fn intword_public_basic() {
        assert_eq!(intword(1_200_000.0), "1.2 million");
        assert_eq!(intword(1_000_000_000.0), "1.0 billion");
        assert_eq!(intword(2_500_000_000_000.0), "2.5 trillion");
    }

    #[test]
    fn intword_public_below_million_unformatted() {
        assert_eq!(intword(999_999.0), "999999");
        assert_eq!(intword(0.0), "0");
        assert_eq!(intword(42.0), "42");
    }

    #[test]
    fn intword_public_negative() {
        assert_eq!(intword(-1_500_000.0), "-1.5 million");
    }

    #[test]
    fn naturalsize_public_basic() {
        assert_eq!(naturalsize(0.0), "0 bytes");
        assert_eq!(naturalsize(1.0), "1 byte");
        assert_eq!(naturalsize(512.0), "512 bytes");
        assert_eq!(naturalsize(1024.0), "1.0 KB");
        assert_eq!(naturalsize(1_572_864.0), "1.5 MB");
    }

    #[test]
    fn naturalsize_public_top_scale_caps() {
        // 2^80 bytes lands on YB, the last unit.
        let n = 1024.0_f64.powi(8);
        let out = naturalsize(n);
        assert!(out.ends_with("YB"), "got: {out}");
    }

    // -------- naturalsize_si (1000-base) --------

    #[test]
    fn naturalsize_si_basic() {
        assert_eq!(naturalsize_si(0.0), "0 bytes");
        assert_eq!(naturalsize_si(1.0), "1 byte");
        assert_eq!(naturalsize_si(512.0), "512 bytes");
        assert_eq!(naturalsize_si(1000.0), "1.0 kB");
        assert_eq!(naturalsize_si(1_500_000.0), "1.5 MB");
    }

    #[test]
    fn naturalsize_si_uses_lowercase_k_for_kilo() {
        // SI writes kilo with a lowercase `k`.
        let out = naturalsize_si(2500.0);
        assert!(out.contains("kB"), "got: {out}");
        assert!(!out.contains("KB"), "must not use uppercase K: {out}");
    }

    #[test]
    fn naturalsize_si_distinct_from_naturalsize_at_boundary() {
        // 1024 bytes: one unit on the 1024 scale, 1.024 rounded on
        // the 1000 scale.
        assert_eq!(naturalsize(1024.0), "1.0 KB");
        assert_eq!(naturalsize_si(1024.0), "1.0 kB");
        // At 1000 bytes they split: the 1000 scale turns over, the
        // 1024 scale is still counting bytes.
        assert_eq!(naturalsize_si(1000.0), "1.0 kB");
        assert_eq!(naturalsize(1000.0), "1000 bytes");
    }

    #[test]
    fn naturalsize_si_top_scale_caps() {
        // 10^24 bytes lands on YB, the last unit.
        let n = 1000.0_f64.powi(8);
        let out = naturalsize_si(n);
        assert!(out.ends_with("YB"), "got: {out}");
    }

    #[test]
    fn ordinal_public_basic() {
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(4), "4th");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(13), "13th");
        assert_eq!(ordinal(21), "21st");
        assert_eq!(ordinal(102), "102nd");
        assert_eq!(ordinal(113), "113th");
    }

    #[test]
    fn ordinal_public_negative_uses_abs_suffix() {
        assert_eq!(ordinal(-1), "-1st");
        assert_eq!(ordinal(-11), "-11th");
        assert_eq!(ordinal(-23), "-23rd");
    }

    #[test]
    fn apnumber_public_basic() {
        assert_eq!(apnumber(1), "one");
        assert_eq!(apnumber(5), "five");
        assert_eq!(apnumber(9), "nine");
        assert_eq!(apnumber(0), "0");
        assert_eq!(apnumber(10), "10");
        assert_eq!(apnumber(-3), "-3");
        assert_eq!(apnumber(100), "100");
    }

    // -------- Public naturaltime / naturalday --------

    fn ntime_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap()
    }

    #[test]
    fn naturaltime_now_within_30s() {
        let now = ntime_now();
        assert_eq!(naturaltime(now, now), "now");
        assert_eq!(naturaltime(now, now + Duration::seconds(20)), "now");
        assert_eq!(naturaltime(now, now - Duration::seconds(29)), "now");
    }

    #[test]
    fn naturaltime_seconds_ago_and_from_now() {
        let now = ntime_now();
        assert_eq!(
            naturaltime(now, now - Duration::seconds(45)),
            "45 seconds ago"
        );
        assert_eq!(
            naturaltime(now, now + Duration::seconds(45)),
            "in 45 seconds"
        );
    }

    #[test]
    fn naturaltime_singular_pluralization() {
        let now = ntime_now();
        assert_eq!(naturaltime(now, now - Duration::minutes(1)), "1 minute ago");
        assert_eq!(naturaltime(now, now + Duration::hours(1)), "in 1 hour");
    }

    #[test]
    fn naturaltime_bucket_transitions() {
        let now = ntime_now();
        assert_eq!(
            naturaltime(now, now - Duration::minutes(3)),
            "3 minutes ago"
        );
        assert_eq!(naturaltime(now, now - Duration::hours(5)), "5 hours ago");
        assert_eq!(naturaltime(now, now - Duration::days(10)), "10 days ago");
        // 35 days → 1 month
        assert_eq!(naturaltime(now, now - Duration::days(35)), "1 month ago");
        // 400 days → 1 year
        assert_eq!(naturaltime(now, now - Duration::days(400)), "1 year ago");
    }

    #[test]
    fn naturalday_today_yesterday_tomorrow() {
        let now = ntime_now();
        assert_eq!(naturalday(now, now), "today");
        assert_eq!(naturalday(now, now - Duration::days(1)), "yesterday");
        assert_eq!(naturalday(now, now + Duration::days(1)), "tomorrow");
    }

    #[test]
    fn naturalday_fallback_format() {
        let now = ntime_now();
        let other = now - Duration::days(45);
        let out = naturalday(now, other);
        // 45 days before 2026-06-05 is 2026-04-21.
        assert_eq!(out, "Apr 21");
    }

    // -------- Public intcomma / intcomma_f64 --------

    #[test]
    fn intcomma_public_basic() {
        assert_eq!(intcomma(0), "0");
        assert_eq!(intcomma(999), "999");
        assert_eq!(intcomma(4500), "4,500");
        assert_eq!(intcomma(1_234_567), "1,234,567");
    }

    #[test]
    fn intcomma_public_negative() {
        assert_eq!(intcomma(-1_000), "-1,000");
        assert_eq!(intcomma(-1_234_567), "-1,234,567");
    }

    #[test]
    fn intcomma_f64_public_basic() {
        assert_eq!(intcomma_f64(1234567.89), "1,234,567.89");
        assert_eq!(intcomma_f64(0.5), "0.5");
        assert_eq!(intcomma_f64(1000.0), "1,000");
    }

    #[test]
    fn intcomma_f64_public_negative() {
        assert_eq!(intcomma_f64(-1000.25), "-1,000.25");
    }

    // -------- Public format_number --------

    #[test]
    fn format_number_en_locale() {
        assert_eq!(format_number(1234567.89, "en", None), "1,234,567.89");
        assert_eq!(format_number(1234567.89, "en-US", None), "1,234,567.89");
        assert_eq!(format_number(1234567.89, "en-GB", None), "1,234,567.89");
    }

    #[test]
    fn format_number_de_locale() {
        assert_eq!(format_number(1234567.89, "de", None), "1.234.567,89");
    }

    #[test]
    fn format_number_fr_locale_space_thousands() {
        assert_eq!(format_number(1234567.89, "fr", None), "1 234 567,89");
    }

    #[test]
    fn format_number_with_decimals() {
        // `Some(n)` pads or cuts to exactly n digits.
        assert_eq!(format_number(1234.5, "en", Some(2)), "1,234.50");
        assert_eq!(format_number(0.0, "en", Some(0)), "0");
        assert_eq!(format_number(1234.56789, "en", Some(2)), "1,234.57"); // rounds
    }

    #[test]
    fn format_number_negative() {
        assert_eq!(format_number(-1234.5, "en", None), "-1,234.5");
        assert_eq!(format_number(-1234.5, "de", Some(2)), "-1.234,50");
    }

    #[test]
    fn format_number_public_unknown_locale_falls_back_to_en() {
        // An unknown locale gives the en-US shape.
        assert_eq!(format_number(1234.5, "xx-YY", None), "1,234.5");
    }

    // -------- Public format_currency --------

    #[test]
    fn format_currency_usd_en_prefix() {
        assert_eq!(format_currency(1234.56, "USD", "en"), "$1,234.56");
        assert_eq!(format_currency(0.0, "USD", "en"), "$0.00");
    }

    #[test]
    fn format_currency_eur_de_prefix() {
        // German puts the `€` first.
        assert_eq!(format_currency(1234.56, "EUR", "de"), "€1.234,56");
    }

    #[test]
    fn format_currency_eur_fr_suffix_with_space() {
        // Romance languages put the `€` last, after a space.
        assert_eq!(format_currency(1234.56, "EUR", "fr"), "1 234,56 €");
        assert_eq!(format_currency(1234.56, "EUR", "it"), "1.234,56 €");
        assert_eq!(format_currency(1234.56, "EUR", "es"), "1.234,56 €");
    }

    #[test]
    fn format_currency_jpy_zero_decimals() {
        // JPY, KRW and CLP show no decimals.
        assert_eq!(format_currency(1234.0, "JPY", "ja"), "¥1,234");
        assert_eq!(format_currency(1234.99, "JPY", "ja"), "¥1,235"); // rounds
    }

    #[test]
    fn format_currency_negative_sign_before_symbol() {
        // The minus sign comes first, before the symbol.
        assert_eq!(format_currency(-50.0, "USD", "en"), "-$50.00");
        // It still leads when the symbol trails.
        assert_eq!(format_currency(-50.0, "EUR", "fr"), "-50,00 €");
    }

    #[test]
    fn format_currency_gbp_uses_pound_symbol() {
        assert_eq!(format_currency(99.99, "GBP", "en-GB"), "£99.99");
    }

    // -------- format_duration_long / format_duration_short --------

    #[test]
    fn format_duration_long_zero() {
        assert_eq!(
            format_duration_long(chrono::Duration::seconds(0)),
            "0 seconds"
        );
    }

    #[test]
    fn format_duration_long_simple_units() {
        assert_eq!(
            format_duration_long(chrono::Duration::seconds(45)),
            "45 seconds"
        );
        assert_eq!(
            format_duration_long(chrono::Duration::seconds(60)),
            "1 minute"
        );
        assert_eq!(
            format_duration_long(chrono::Duration::seconds(125)),
            "2 minutes, 5 seconds"
        );
    }

    #[test]
    fn format_duration_long_multiple_units() {
        assert_eq!(
            format_duration_long(chrono::Duration::seconds(3725)),
            "1 hour, 2 minutes, 5 seconds"
        );
        assert_eq!(
            format_duration_long(chrono::Duration::days(2) + chrono::Duration::hours(3)),
            "2 days, 3 hours"
        );
    }

    #[test]
    fn format_duration_long_negative() {
        assert_eq!(
            format_duration_long(chrono::Duration::minutes(-5)),
            "-5 minutes"
        );
    }

    #[test]
    fn format_duration_short_zero() {
        assert_eq!(format_duration_short(chrono::Duration::seconds(0)), "0s");
    }

    #[test]
    fn format_duration_short_compact_shape() {
        assert_eq!(format_duration_short(chrono::Duration::seconds(45)), "45s");
        assert_eq!(
            format_duration_short(chrono::Duration::seconds(125)),
            "2m5s"
        );
        assert_eq!(
            format_duration_short(chrono::Duration::seconds(3725)),
            "1h2m5s"
        );
        assert_eq!(
            format_duration_short(chrono::Duration::days(2) + chrono::Duration::hours(3)),
            "2d3h"
        );
    }

    #[test]
    fn format_duration_short_negative() {
        assert_eq!(format_duration_short(chrono::Duration::minutes(-5)), "-5m");
    }

    #[test]
    fn format_duration_long_tera_filter_accepts_int_seconds() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("secs", &3725_i64);
        assert_eq!(
            render(&tera, "{{ secs | format_duration_long }}", ctx),
            "1 hour, 2 minutes, 5 seconds"
        );
    }

    #[test]
    fn format_duration_short_tera_filter_accepts_int_seconds() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("secs", &3725_i64);
        assert_eq!(
            render(&tera, "{{ secs | format_duration_short }}", ctx),
            "1h2m5s"
        );
    }

    #[test]
    fn format_duration_tera_filter_accepts_iso8601() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("d", "PT1H2M5S");
        assert_eq!(
            render(&tera, "{{ d | format_duration_long }}", ctx),
            "1 hour, 2 minutes, 5 seconds"
        );
    }

    #[test]
    fn format_duration_tera_filter_passes_through_unparseable() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("garbage", "not a duration");
        assert_eq!(
            render(&tera, "{{ garbage | format_duration_long }}", ctx),
            "not a duration"
        );
    }

    // -------- naturaltime_short --------

    fn fixed_now() -> chrono::DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 6, 5, 12, 0, 0).unwrap()
    }

    #[test]
    fn naturaltime_short_within_30s_returns_now() {
        let now = fixed_now();
        assert_eq!(naturaltime_short(now, now), "now");
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::seconds(15)),
            "now"
        );
    }

    #[test]
    fn naturaltime_short_seconds_minutes_hours() {
        let now = fixed_now();
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::seconds(45)),
            "45s ago"
        );
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::minutes(5)),
            "5m ago"
        );
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::hours(3)),
            "3h ago"
        );
    }

    #[test]
    fn naturaltime_short_days_months_years() {
        let now = fixed_now();
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::days(2)),
            "2d ago"
        );
        // 100 days ≈ 3.3 months → "3mo ago".
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::days(100)),
            "3mo ago"
        );
        // 800 days ≈ 2.2 years → "2y ago".
        assert_eq!(
            naturaltime_short(now, now - chrono::Duration::days(800)),
            "2y ago"
        );
    }

    #[test]
    fn naturaltime_short_future_uses_in_prefix() {
        let now = fixed_now();
        assert_eq!(
            naturaltime_short(now, now + chrono::Duration::minutes(10)),
            "in 10m"
        );
        assert_eq!(
            naturaltime_short(now, now + chrono::Duration::hours(5)),
            "in 5h"
        );
    }

    #[test]
    fn naturaltime_short_tera_filter() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("ts", "2020-01-01T00:00:00Z");
        let out = render(&tera, "{{ ts | naturaltime_short }}", ctx);
        // Many years in the past — must contain "y ago".
        assert!(out.contains("y ago"), "got: {out}");
    }
}
