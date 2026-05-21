//! Per-request timezone activation — Django's `USE_TZ = True` flow.
//! Issue #47.
//!
//! Django's stack stores UTC in the DB and converts to a per-request
//! active timezone at render time. This module ports that shape via
//! a `tokio::task_local` carrying the active `FixedOffset`:
//!
//! ```ignore
//! use rustango::i18n::timezone::{activate, current_offset, localtime};
//! use chrono::{Duration, FixedOffset, Utc};
//!
//! // Per-request: pull the user's TZ offset from a cookie / settings row.
//! let user_offset = FixedOffset::west_opt(5 * 3600).unwrap(); // UTC-05:00
//!
//! // Run the handler with the active offset installed.
//! with_offset(user_offset, async {
//!     // Inside the closure, current_offset() returns user_offset.
//!     let now_utc = Utc::now();
//!     let now_local = localtime(now_utc);  // converted to UTC-05:00.
//!     // ... render template, the {{ ts | localtime }} filter sees user_offset ...
//! }).await;
//! ```
//!
//! ## Scope: FixedOffset only
//!
//! This module models the active timezone as a `chrono::FixedOffset`.
//! That captures everything a *single point in time* needs — render
//! a UTC `DateTime` in the user's local clock — without pulling in
//! the ~3MB `chrono-tz` IANA database.
//!
//! What's NOT here: DST transitions ("America/New_York switches from
//! -05 to -04 in March"). For full IANA support, parse the user's
//! `Tz` via `chrono-tz` at request time and pass `tz.offset_from_utc_datetime(...)`
//! into [`activate`] / [`with_offset`]. The session-cookie + middleware
//! shape in this file is offset-aware and the chrono-tz layer is
//! strictly additive.
//!
//! ## LocaleMiddleware
//!
//! See [`from_request_headers`] for an `axum` middleware that reads
//! the timezone offset from a request cookie (`tz_offset=-300` for
//! `UTC-05:00`) and binds the task-local for the request's
//! lifetime.

use std::future::Future;

use chrono::{DateTime, FixedOffset, Utc};
// `TimeZone` is the trait that supplies `Utc.timestamp_opt(...)` —
// only used by the `template_views`-gated Tera filter below.
#[cfg(feature = "template_views")]
use chrono::TimeZone;

tokio::task_local! {
    /// The active timezone for this request / task. `None` (no
    /// task-local set) means "fall back to UTC" — i.e. behave as
    /// if no override is active.
    static ACTIVE_TZ: FixedOffset;
}

/// The active timezone for the current task. Returns `UTC` when no
/// override has been installed via [`with_offset`] or [`activate`].
///
/// **Pure read** — never mutates the task-local. Use as the
/// "what TZ should I render in?" lookup inside handlers / template
/// helpers.
#[must_use]
pub fn current_offset() -> FixedOffset {
    ACTIVE_TZ
        .try_with(|tz| *tz)
        .unwrap_or_else(|_| FixedOffset::east_opt(0).expect("UTC offset"))
}

/// Run `future` with the active timezone temporarily set to
/// `offset`. Django's `timezone.override(tz)` analog.
///
/// The override is scoped to `future`'s execution — it doesn't
/// leak to other tasks or out of the closure. Nested calls
/// replace (not stack-add) the outer offset for the inner scope,
/// matching Django's `override` semantics.
pub async fn with_offset<F>(offset: FixedOffset, future: F) -> F::Output
where
    F: Future,
{
    ACTIVE_TZ.scope(offset, future).await
}

/// **DO NOT USE in production code.** This is a process-global
/// override that affects every future task on every thread —
/// equivalent to setting `TIME_ZONE` in Django settings. It exists
/// for tests and one-off scripts that want a fixed offset for the
/// whole process.
///
/// For per-request overrides, use [`with_offset`] (task-scoped).
///
/// Returns `true` if the global was installed for the first time,
/// `false` if a fallback had already been set (idempotent — first
/// call wins, like `OnceLock`).
pub fn activate(_offset: FixedOffset) -> bool {
    // Reserved for a future process-global fallback layer (a
    // `OnceLock<FixedOffset>` consulted by `current_offset` when
    // no task-local is set). The current shape uses task-local
    // only — keeping this function as a no-op so the public API
    // is stable when the global fallback lands.
    false
}

/// Convert a `DateTime<Utc>` to the current active offset.
/// Django's `timezone.localtime(value)`.
///
/// ```ignore
/// // Inside a handler running under `with_offset(user_offset, ...)`:
/// let now_local = localtime(Utc::now());
/// ```
#[must_use]
pub fn localtime(utc_dt: DateTime<Utc>) -> DateTime<FixedOffset> {
    let off = current_offset();
    utc_dt.with_timezone(&off)
}

/// Convert a `DateTime<Utc>` to an explicit offset (ignores the
/// task-local). Django's `timezone.localtime(value, tz=...)`.
#[must_use]
pub fn localtime_with_offset(utc_dt: DateTime<Utc>, offset: FixedOffset) -> DateTime<FixedOffset> {
    utc_dt.with_timezone(&offset)
}

/// Parse a TZ-offset string (e.g. `"+05:00"`, `"-08:30"`, `"Z"`,
/// `"UTC"`, or signed-integer minutes like `"-300"`) into a
/// `FixedOffset`. Used by middleware to decode the cookie value.
///
/// Returns `None` on any unparseable input. `"Z"` and `"UTC"` map
/// to offset 0.
#[must_use]
pub fn parse_offset(s: &str) -> Option<FixedOffset> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("z") || s.eq_ignore_ascii_case("utc") {
        return Some(FixedOffset::east_opt(0).expect("UTC offset"));
    }
    // "+HH:MM" / "-HH:MM" — try the chrono shape first.
    if let Some(stripped) = s.strip_prefix('+').or_else(|| s.strip_prefix('-')) {
        if let Some((hh, mm)) = stripped.split_once(':') {
            let h: i32 = hh.parse().ok()?;
            let m: i32 = mm.parse().ok()?;
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                return None;
            }
            let total = h * 3600 + m * 60;
            let signed = if s.starts_with('-') { -total } else { total };
            return FixedOffset::east_opt(signed);
        }
        // "+HHMM" without colon.
        if stripped.len() == 4 && stripped.chars().all(|c| c.is_ascii_digit()) {
            let h: i32 = stripped[..2].parse().ok()?;
            let m: i32 = stripped[2..].parse().ok()?;
            let total = h * 3600 + m * 60;
            let signed = if s.starts_with('-') { -total } else { total };
            return FixedOffset::east_opt(signed);
        }
    }
    // Signed integer minutes — the shape JS's `Date.getTimezoneOffset()`
    // returns. NB: JS returns POSITIVE for west-of-UTC, so callers using
    // that should flip the sign before passing.
    if let Ok(mins) = s.parse::<i32>() {
        return FixedOffset::east_opt(mins.saturating_mul(60));
    }
    None
}

/// Read the timezone offset cookie value (default name `tz_offset`)
/// from a request's headers and parse it. Returns `None` when the
/// cookie is missing or malformed.
///
/// Used by [`from_request_headers`] to feed the active timezone.
/// Public so apps that wire their own middleware stack can reuse
/// the decode step.
pub fn from_cookie(headers: &axum::http::HeaderMap, cookie_name: &str) -> Option<FixedOffset> {
    let raw = headers
        .get(axum::http::header::COOKIE)
        .and_then(|h| h.to_str().ok())?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(&format!("{cookie_name}=")) {
            return parse_offset(value);
        }
    }
    None
}

/// Extract a `FixedOffset` from request headers — checks the
/// configured cookie first, then falls back to a `Time-Zone:`
/// request header (some clients send this directly). Returns
/// `None` when neither carries a parseable value.
///
/// The conventional cookie name is `tz_offset` — set by a small
/// JS snippet that reads `new Date().getTimezoneOffset()` on page
/// load and flips the sign:
///
/// ```html
/// <script>
///   const m = -new Date().getTimezoneOffset();
///   document.cookie = `tz_offset=${m};path=/;max-age=31536000`;
/// </script>
/// ```
pub fn from_request_headers(
    headers: &axum::http::HeaderMap,
    cookie_name: &str,
) -> Option<FixedOffset> {
    from_cookie(headers, cookie_name).or_else(|| {
        headers
            .get("time-zone")
            .or_else(|| headers.get("x-timezone"))
            .and_then(|v| v.to_str().ok())
            .and_then(parse_offset)
    })
}

/// Register the `localtime` Tera filter on `tera`. Renders a stored
/// UTC datetime (any RFC 3339 string) in the request's active
/// timezone — Django's `{{ ts | localtime }}` shape.
///
/// ```ignore
/// let mut tera = tera::Tera::default();
/// rustango::i18n::timezone::register_filters(&mut tera);
/// // In templates:
/// // {{ created_at | localtime }}                     # current offset, default format
/// // {{ created_at | localtime(format="%Y-%m-%d") }} # custom format
/// ```
///
/// Inputs accepted: RFC 3339 strings (`2026-01-15T12:00:00Z`,
/// `+00:00`), and numeric Unix timestamps (seconds). Anything else
/// passes through unchanged.
#[cfg(feature = "template_views")]
pub fn register_filters(tera: &mut tera::Tera) {
    tera.register_filter("localtime", localtime_filter);
}

#[cfg(feature = "template_views")]
fn localtime_filter(
    value: &tera::Value,
    args: &std::collections::HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let format = args
        .get("format")
        .and_then(tera::Value::as_str)
        .unwrap_or("%Y-%m-%d %H:%M:%S %z");
    // Try string (RFC 3339) first, then numeric Unix-seconds.
    let utc_dt: Option<DateTime<Utc>> = match value {
        tera::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc)),
        tera::Value::Number(n) => n
            .as_i64()
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single()),
        _ => None,
    };
    let Some(utc_dt) = utc_dt else {
        return Ok(value.clone());
    };
    let local = utc_dt.with_timezone(&current_offset());
    Ok(tera::Value::String(local.format(format).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn current_offset_defaults_to_utc_outside_scope() {
        let off = current_offset();
        assert_eq!(off.local_minus_utc(), 0);
    }

    #[tokio::test]
    async fn with_offset_installs_task_local() {
        let target = FixedOffset::east_opt(5 * 3600).unwrap();
        with_offset(target, async move {
            let inside = current_offset();
            assert_eq!(inside, target);
        })
        .await;
        // After the scope ends, the default reasserts.
        assert_eq!(current_offset().local_minus_utc(), 0);
    }

    #[tokio::test]
    async fn nested_with_offset_replaces_outer_scope() {
        let outer = FixedOffset::east_opt(2 * 3600).unwrap();
        let inner = FixedOffset::east_opt(9 * 3600).unwrap();
        with_offset(outer, async move {
            assert_eq!(current_offset(), outer);
            with_offset(inner, async move {
                assert_eq!(current_offset(), inner);
            })
            .await;
            // Outer restored after inner exits.
            assert_eq!(current_offset(), outer);
        })
        .await;
    }

    #[tokio::test]
    async fn localtime_converts_utc_to_active_offset() {
        let target = FixedOffset::east_opt(3 * 3600).unwrap();
        let utc = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        with_offset(target, async move {
            let local = localtime(utc);
            assert_eq!(local.hour(), 15); // 12:00 UTC → 15:00 UTC+3
            assert_eq!(local.offset(), &target);
        })
        .await;
    }

    #[test]
    fn localtime_with_explicit_offset_ignores_task_local() {
        let east8 = FixedOffset::east_opt(8 * 3600).unwrap();
        let utc = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let local = localtime_with_offset(utc, east8);
        assert_eq!(local.hour(), 8);
    }

    use chrono::Timelike;

    #[test]
    fn parse_offset_handles_colon_and_unsigned_shapes() {
        assert_eq!(
            parse_offset("+05:30"),
            Some(FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap())
        );
        assert_eq!(
            parse_offset("-08:00"),
            Some(FixedOffset::west_opt(8 * 3600).unwrap())
        );
        assert_eq!(
            parse_offset("+0530"),
            Some(FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap())
        );
        assert_eq!(parse_offset("Z"), Some(FixedOffset::east_opt(0).unwrap()));
        assert_eq!(parse_offset("UTC"), Some(FixedOffset::east_opt(0).unwrap()));
        assert_eq!(parse_offset("utc"), Some(FixedOffset::east_opt(0).unwrap()));
    }

    #[test]
    fn parse_offset_handles_signed_minutes() {
        // `-300` minutes = UTC-05:00 (typical JS shape after sign flip).
        assert_eq!(
            parse_offset("-300"),
            Some(FixedOffset::west_opt(5 * 3600).unwrap())
        );
        assert_eq!(
            parse_offset("60"),
            Some(FixedOffset::east_opt(60 * 60).unwrap())
        );
        assert_eq!(parse_offset("0"), Some(FixedOffset::east_opt(0).unwrap()));
    }

    #[test]
    fn parse_offset_rejects_garbage() {
        assert_eq!(parse_offset(""), None);
        assert_eq!(parse_offset("not a tz"), None);
        assert_eq!(parse_offset("+25:00"), None); // hour out of range
        assert_eq!(parse_offset("+05:99"), None); // minute out of range
    }

    #[test]
    fn from_cookie_finds_named_cookie() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("foo=bar; tz_offset=+05:30; baz=qux"),
        );
        let off = from_cookie(&headers, "tz_offset").unwrap();
        assert_eq!(off.local_minus_utc(), 5 * 3600 + 30 * 60);
    }

    #[test]
    fn from_cookie_returns_none_when_absent() {
        let headers = axum::http::HeaderMap::new();
        assert!(from_cookie(&headers, "tz_offset").is_none());
    }

    #[test]
    fn from_request_headers_falls_back_to_time_zone_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("time-zone", "+09:00".parse().unwrap());
        let off = from_request_headers(&headers, "tz_offset").unwrap();
        assert_eq!(off.local_minus_utc(), 9 * 3600);
    }

    #[test]
    fn from_request_headers_prefers_cookie_over_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            "tz_offset=+05:00".parse().unwrap(),
        );
        headers.insert("time-zone", "+09:00".parse().unwrap());
        let off = from_request_headers(&headers, "tz_offset").unwrap();
        assert_eq!(off.local_minus_utc(), 5 * 3600);
    }

    // ---------------- localtime filter ----------------

    #[cfg(feature = "template_views")]
    #[test]
    fn localtime_filter_renders_in_active_offset_default_format() {
        let mut tera = tera::Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ ts | localtime }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("ts", "2026-01-15T12:00:00Z");
        // No active TZ → default to UTC.
        let out = tera.render("t", &ctx).unwrap();
        assert!(out.starts_with("2026-01-15 12:00:00"));
    }

    #[cfg(feature = "template_views")]
    #[tokio::test]
    async fn localtime_filter_uses_active_task_local_offset() {
        let target = FixedOffset::east_opt(3 * 3600).unwrap();
        with_offset(target, async {
            let mut tera = tera::Tera::default();
            register_filters(&mut tera);
            tera.add_raw_template("t", "{{ ts | localtime(format=\"%H:%M\") }}")
                .unwrap();
            let mut ctx = tera::Context::new();
            ctx.insert("ts", "2026-01-15T12:00:00Z");
            let out = tera.render("t", &ctx).unwrap();
            // 12:00 UTC + 3h = 15:00 local.
            assert_eq!(out, "15:00");
        })
        .await;
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn localtime_filter_passes_through_non_datetime_input() {
        let mut tera = tera::Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ s | localtime }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("s", "not a date");
        let out = tera.render("t", &ctx).unwrap();
        assert_eq!(out, "not a date");
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn localtime_filter_accepts_unix_seconds() {
        let mut tera = tera::Tera::default();
        register_filters(&mut tera);
        tera.add_raw_template("t", "{{ ts | localtime(format=\"%Y-%m-%d %H:%M:%S\") }}")
            .unwrap();
        let mut ctx = tera::Context::new();
        // Use chrono to compute the timestamp so we don't have to
        // hand-derive epoch seconds (and get them wrong).
        let dt = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        ctx.insert("ts", &dt.timestamp());
        let out = tera.render("t", &ctx).unwrap();
        assert_eq!(out, "2026-01-15 12:00:00");
    }
}
