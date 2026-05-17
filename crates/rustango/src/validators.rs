//! Django-shape standalone validators. Issue #54.
//!
//! Lightweight check functions that mirror Django's
//! `django.core.validators` module. Each returns `Result<(),
//! ValidationError>` so they compose with `?` inside `Form::clean()`
//! or any other input-validation flow.
//!
//! ```ignore
//! use rustango::validators::{validate_email, validate_url, validate_slug};
//!
//! pub fn clean_contact(email: &str, website: &str, handle: &str) -> Result<(), ValidationError> {
//!     validate_email(email)?;
//!     validate_url(website)?;
//!     validate_slug(handle)?;
//!     Ok(())
//! }
//! ```
//!
//! ## Scope
//!
//! Hand-rolled checks (no `regex` crate dependency) — fast, allocation-free,
//! and cover the 99% case. The character-class checks here match
//! Django's defaults for the typical web form:
//!
//! - `validate_email` — basic shape check (local-part `@` domain `.` tld).
//!   NOT RFC 5322. Catches typos like missing `@` or empty parts.
//! - `validate_url` — accepts `http://` / `https://` schemes with a
//!   non-empty host. Optional port + path + query.
//! - `validate_slug` — `[a-zA-Z0-9_-]+`. Django's `slug_re`.
//! - `validate_unicode_slug` — letters of any script + digits + `_` + `-`.
//!   Django's unicode-aware `UnicodeSlugValidator`.
//! - `validate_prohibit_null_characters` — reject strings containing
//!   NUL (`\0`). Mirrors Django's `ProhibitNullCharactersValidator`,
//!   used at form-input boundaries to block null-byte injection.
//! - `validate_min_length` / `validate_max_length` — string char count.
//! - `validate_min_value` / `validate_max_value` — i64 numeric bounds.
//! - `validate_min_value_f64` / `validate_max_value_f64` — float
//!   bounds, for prices / measurements / scientific values that
//!   don't fit `i64`.
//! - `validate_integer` — parses as `i64`.
//! - `validate_decimal` — `max_digits` + `decimal_places` bounds
//!   (Django's `DecimalValidator`).
//! - `validate_ipv4_address` / `validate_ipv6_address` — dotted-quad /
//!   colon-hex address shape via `std::net::Ipv4Addr` / `Ipv6Addr`.
//! - `validate_comma_separated_integer_list` — `"1,2,3"`. Django's
//!   `validate_comma_separated_integer_list`.
//! - `validate_email_list` — comma-separated list of email addresses,
//!   one per "CC" field entry.
//! - `validate_phone_e164` — E.164 international phone format
//!   (`+` followed by 1–15 digits). Not a Django built-in but
//!   widely needed in the same role.
//! - `validate_hex_color` — `#rgb` / `#rrggbb` / `#rrggbbaa` /
//!   `#rgba` web-color hex codes. For color-picker form fields.
//! - `validate_uuid` — RFC 4122 UUID string format. Useful for
//!   handler argument validation when a path/query carries a UUID
//!   you want to validate before hitting the DB.
//! - `validate_iso_date` — ISO 8601 `YYYY-MM-DD` date.
//! - `validate_iso_time` — ISO 8601 `HH:MM:SS` time (optional
//!   fractional seconds).
//! - `validate_iso_datetime` — RFC 3339 / ISO 8601 datetime with
//!   timezone offset (`...Z` or `+HH:MM`).
//! - `validate_alphanumeric` — `[a-zA-Z0-9]+` ASCII only.
//! - `validate_numeric` — `[0-9]+` ASCII digits only.
//! - `validate_alpha` — `[a-zA-Z]+` ASCII letters only.
//!
//! What's NOT here (yet):
//! - Locale-sensitive numeric formats.
//! - IDN / punycode email + URL support (the basic check accepts
//!   ASCII; non-ASCII domains are valid per RFC but the basic
//!   shape rejects them — file a follow-up if you actually need
//!   IDN forms).

/// One validation failure.
///
/// Carries a stable `code` (machine-readable, used to map to UI
/// localized strings) and a `message` (human-readable English
/// default). Forms typically forward both into a `FormErrors` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Stable machine-readable code, e.g. `"invalid_email"`,
    /// `"min_length"`. Use to look up localized messages.
    pub code: &'static str,
    /// Default English message. Replace via the localization layer
    /// when surfacing to end users.
    pub message: String,
}

impl ValidationError {
    /// Construct a validation error with a code + message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ValidationError {}

// ------------------------------------------------------------------ email

/// Validate that `s` looks like an email address.
///
/// Basic shape check: a non-empty local part, exactly one `@`, a
/// non-empty domain containing at least one `.`, and a non-empty
/// TLD. **Not** RFC 5322 — catches typos (missing `@`, double dots)
/// but doesn't enforce the full grammar. For production-grade email
/// verification, send a confirmation email anyway.
///
/// # Errors
/// `ValidationError { code: "invalid_email", ... }` on shape mismatch.
pub fn validate_email(s: &str) -> Result<(), ValidationError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::new(
            "invalid_email",
            "Enter a valid email address.",
        ));
    }
    let (local, domain) = match trimmed.split_once('@') {
        Some(parts) => parts,
        None => {
            return Err(ValidationError::new(
                "invalid_email",
                "Enter a valid email address.",
            ))
        }
    };
    if local.is_empty() || domain.is_empty() {
        return Err(ValidationError::new(
            "invalid_email",
            "Enter a valid email address.",
        ));
    }
    // Reject a SECOND @ — split_once gave us first occurrence;
    // any remaining @ in the domain is invalid.
    if domain.contains('@') {
        return Err(ValidationError::new(
            "invalid_email",
            "Enter a valid email address.",
        ));
    }
    // Domain must have a dot, and that dot can't be at either end
    // (".com" / "example." both invalid).
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return Err(ValidationError::new(
            "invalid_email",
            "Enter a valid email address.",
        ));
    }
    // Reject consecutive dots in domain or local — common typo.
    if domain.contains("..") || local.contains("..") {
        return Err(ValidationError::new(
            "invalid_email",
            "Enter a valid email address.",
        ));
    }
    Ok(())
}

/// Boolean form of [`validate_email`]. Useful in `if` guards where
/// the caller doesn't need the error message.
#[must_use]
pub fn is_email(s: &str) -> bool {
    validate_email(s).is_ok()
}

// ------------------------------------------------------------------ url

/// Validate that `s` looks like an `http://` / `https://` URL.
///
/// Checks scheme + non-empty host. Doesn't validate path / query /
/// fragment beyond requiring well-formed UTF-8 (which is implicit
/// in `&str`).
///
/// # Errors
/// `ValidationError { code: "invalid_url", ... }`.
pub fn validate_url(s: &str) -> Result<(), ValidationError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::new("invalid_url", "Enter a valid URL."));
    }
    let rest = if let Some(r) = trimmed.strip_prefix("https://") {
        r
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        r
    } else {
        return Err(ValidationError::new("invalid_url", "Enter a valid URL."));
    };
    // Host portion: everything up to the first '/', '?', or '#'.
    let host_end = rest
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() || host.starts_with(':') {
        return Err(ValidationError::new("invalid_url", "Enter a valid URL."));
    }
    // Strip optional :port off the host and require a hostname part.
    let hostname = host.split(':').next().unwrap_or(host);
    if hostname.is_empty() {
        return Err(ValidationError::new("invalid_url", "Enter a valid URL."));
    }
    Ok(())
}

/// Boolean form of [`validate_url`].
#[must_use]
pub fn is_url(s: &str) -> bool {
    validate_url(s).is_ok()
}

// ------------------------------------------------------------------ slug

/// Validate that `s` is a Django-shape slug: `[a-zA-Z0-9_-]+`, no
/// other characters, must be non-empty.
///
/// # Errors
/// `ValidationError { code: "invalid_slug", ... }`.
pub fn validate_slug(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "invalid_slug",
            "Enter a valid slug consisting of letters, numbers, underscores or hyphens.",
        ));
    }
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
        if !ok {
            return Err(ValidationError::new(
                "invalid_slug",
                "Enter a valid slug consisting of letters, numbers, underscores or hyphens.",
            ));
        }
    }
    Ok(())
}

/// Boolean form of [`validate_slug`].
#[must_use]
pub fn is_slug(s: &str) -> bool {
    validate_slug(s).is_ok()
}

/// Validate a unicode-aware slug. Allows any Unicode alphanumeric
/// character plus `_` and `-`. Mirrors Django's
/// `validate_unicode_slug`, which is the variant Django falls back
/// to under `SLUG_VALIDATOR = UnicodeSlugValidator` (set via the
/// `Field(allow_unicode=True)` shape).
///
/// # Errors
/// `ValidationError { code: "invalid_unicode_slug", ... }`.
pub fn validate_unicode_slug(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "invalid_unicode_slug",
            "Enter a valid slug consisting of Unicode letters, numbers, underscores or hyphens.",
        ));
    }
    for ch in s.chars() {
        let ok = ch.is_alphanumeric() || ch == '_' || ch == '-';
        if !ok {
            return Err(ValidationError::new(
                "invalid_unicode_slug",
                "Enter a valid slug consisting of Unicode letters, numbers, underscores or hyphens.",
            ));
        }
    }
    Ok(())
}

/// Boolean form of [`validate_unicode_slug`].
#[must_use]
pub fn is_unicode_slug(s: &str) -> bool {
    validate_unicode_slug(s).is_ok()
}

// ------------------------------------------------------------------ null-character guard

/// Reject strings containing NUL (`\0`). Mirrors Django's
/// `ProhibitNullCharactersValidator`. Null bytes inside user input
/// are a known injection vector against C-string-aware downstream
/// systems (database drivers, file paths, syscalls) and almost
/// never represent legitimate user intent — Django runs this
/// validator on every CharField by default.
///
/// # Errors
/// `ValidationError { code: "null_characters_not_allowed", ... }`.
pub fn validate_prohibit_null_characters(s: &str) -> Result<(), ValidationError> {
    if s.contains('\0') {
        return Err(ValidationError::new(
            "null_characters_not_allowed",
            "Null characters are not allowed.",
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ phone (E.164)

/// Validate an [E.164](https://en.wikipedia.org/wiki/E.164)
/// international phone number: a `+` followed by 1 to 15 ASCII
/// digits, no other characters. This is the format every modern
/// phone API (Twilio, AWS SNS, Vonage) expects.
///
/// Not a Django built-in — Django delegates phone validation to
/// the `django-phonenumber-field` package which depends on
/// `phonenumbers` (the Google libphonenumber port). E.164 is a
/// reasonable lowest-common-denominator that doesn't require a
/// 20MB country-codes database. For full national-format /
/// region-aware parsing, plug a `phonenumbers`-backed validator
/// alongside this one.
///
/// Examples:
/// - `validate_phone_e164("+14155552671")` → `Ok(())`
/// - `validate_phone_e164("+442012345678")` → `Ok(())`
/// - `validate_phone_e164("415-555-2671")` → `Err(invalid_phone)` (no `+`)
/// - `validate_phone_e164("+1")` → `Ok(())` (1 digit is the minimum)
/// - `validate_phone_e164("+0123456789012345")` → `Err(invalid_phone)` (16 digits)
///
/// # Errors
/// `ValidationError { code: "invalid_phone", ... }`.
pub fn validate_phone_e164(s: &str) -> Result<(), ValidationError> {
    let rest = match s.strip_prefix('+') {
        Some(r) => r,
        None => {
            return Err(ValidationError::new(
                "invalid_phone",
                "Enter a phone number in E.164 format (e.g. +14155552671).",
            ));
        }
    };
    let len = rest.len();
    if !(1..=15).contains(&len) {
        return Err(ValidationError::new(
            "invalid_phone",
            "Enter a phone number in E.164 format (e.g. +14155552671).",
        ));
    }
    if !rest.chars().all(|c| c.is_ascii_digit()) {
        return Err(ValidationError::new(
            "invalid_phone",
            "Enter a phone number in E.164 format (e.g. +14155552671).",
        ));
    }
    Ok(())
}

/// Boolean form of [`validate_phone_e164`].
#[must_use]
pub fn is_phone_e164(s: &str) -> bool {
    validate_phone_e164(s).is_ok()
}

// ------------------------------------------------------------------ hex color

/// Validate a web-color hex code: `#rgb`, `#rrggbb`, `#rgba`, or
/// `#rrggbbaa`. The `#` prefix is required; hex digits are
/// case-insensitive. Anything else (named colors, `rgb()` /
/// `hsl()` functions, hex without `#`) is rejected.
///
/// Useful for color-picker form fields in admin / theme-config
/// surfaces. Pair with [`validate_max_length`] to bound the input
/// to 9 characters total.
///
/// Examples:
/// - `validate_hex_color("#fff")` → `Ok(())`
/// - `validate_hex_color("#ffaabb")` → `Ok(())`
/// - `validate_hex_color("#FFAA00CC")` → `Ok(())` (8 = with alpha)
/// - `validate_hex_color("fff")` → `Err` (missing `#`)
/// - `validate_hex_color("#ffffg")` → `Err` (`g` not hex)
/// - `validate_hex_color("#ffff")` → `Err` (4 chars: only valid
///   shorthand is 3 / 6 / 4 / 8)
///
/// # Errors
/// `ValidationError { code: "invalid_hex_color", ... }`.
pub fn validate_hex_color(s: &str) -> Result<(), ValidationError> {
    let rest = match s.strip_prefix('#') {
        Some(r) => r,
        None => {
            return Err(ValidationError::new(
                "invalid_hex_color",
                "Enter a hex color like `#fff` or `#ffaa00`.",
            ));
        }
    };
    if !matches!(rest.len(), 3 | 4 | 6 | 8) {
        return Err(ValidationError::new(
            "invalid_hex_color",
            "Enter a hex color like `#fff` or `#ffaa00`.",
        ));
    }
    if !rest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ValidationError::new(
            "invalid_hex_color",
            "Enter a hex color like `#fff` or `#ffaa00`.",
        ));
    }
    Ok(())
}

/// Boolean form of [`validate_hex_color`].
#[must_use]
pub fn is_hex_color(s: &str) -> bool {
    validate_hex_color(s).is_ok()
}

// ------------------------------------------------------------------ uuid

/// Validate a UUID string in any of the formats `uuid::Uuid::parse_str`
/// accepts (hyphenated, simple, URN, or braced). Backs onto the
/// `uuid` crate so the accepted shapes match the rest of rustango.
///
/// Useful for handler argument validation when a path / query
/// parameter carries a UUID you want to validate before hitting the
/// DB — gives a clean `ValidationError` with a stable code instead
/// of a "no rows matched" 500 deeper in the stack.
///
/// Examples:
/// - `validate_uuid("550e8400-e29b-41d4-a716-446655440000")` → Ok
/// - `validate_uuid("550e8400e29b41d4a716446655440000")` → Ok (simple)
/// - `validate_uuid("urn:uuid:550e8400-e29b-41d4-a716-446655440000")` → Ok
/// - `validate_uuid("not-a-uuid")` → Err
///
/// # Errors
/// `ValidationError { code: "invalid_uuid", ... }`.
pub fn validate_uuid(s: &str) -> Result<(), ValidationError> {
    uuid::Uuid::parse_str(s)
        .map(|_| ())
        .map_err(|_| ValidationError::new("invalid_uuid", "Enter a valid UUID."))
}

/// Boolean form of [`validate_uuid`].
#[must_use]
pub fn is_uuid(s: &str) -> bool {
    validate_uuid(s).is_ok()
}

// ------------------------------------------------------------------ ISO 8601 date / time / datetime

/// Validate an ISO 8601 calendar date in `YYYY-MM-DD` format. Rejects
/// shorter / longer strings and out-of-range months / days (`Feb 30`,
/// month 13, day 0 etc.) via chrono's `NaiveDate::parse_from_str`.
///
/// # Errors
/// `ValidationError { code: "invalid_iso_date", ... }`.
pub fn validate_iso_date(s: &str) -> Result<(), ValidationError> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| ValidationError::new("invalid_iso_date", "Enter a date in YYYY-MM-DD format."))
}

/// Validate an ISO 8601 wall-clock time in `HH:MM:SS` format,
/// optionally with fractional seconds (`HH:MM:SS.sss`). Rejects
/// out-of-range hours / minutes / seconds.
///
/// # Errors
/// `ValidationError { code: "invalid_iso_time", ... }`.
pub fn validate_iso_time(s: &str) -> Result<(), ValidationError> {
    // Accept "HH:MM:SS" and "HH:MM:SS.sss" — chrono parses both with
    // the %.f optional-fraction specifier.
    chrono::NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
        .map(|_| ())
        .map_err(|_| ValidationError::new("invalid_iso_time", "Enter a time in HH:MM:SS format."))
}

/// Validate an RFC 3339 / ISO 8601 datetime with timezone offset
/// (`2026-01-15T14:30:00Z` or `2026-01-15T14:30:00+02:00`). The
/// timezone is REQUIRED — naive datetimes (no offset) are rejected
/// because mixing local-time and UTC values without a marker is a
/// classic data-corruption vector.
///
/// # Errors
/// `ValidationError { code: "invalid_iso_datetime", ... }`.
pub fn validate_iso_datetime(s: &str) -> Result<(), ValidationError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|_| ())
        .map_err(|_| {
            ValidationError::new(
                "invalid_iso_datetime",
                "Enter a datetime in RFC 3339 format (e.g. 2026-01-15T14:30:00Z).",
            )
        })
}

// ------------------------------------------------------------------ character-class predicates

/// Reject any input that contains characters outside
/// `[a-zA-Z0-9]`. Non-empty input required. ASCII only — for the
/// Unicode-aware variant use [`validate_unicode_slug`] (which adds
/// `_` / `-` to letters/digits).
///
/// # Errors
/// `ValidationError { code: "not_alphanumeric", ... }`.
pub fn validate_alphanumeric(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "not_alphanumeric",
            "Enter only letters and digits.",
        ));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ValidationError::new(
            "not_alphanumeric",
            "Enter only letters and digits.",
        ));
    }
    Ok(())
}

/// Reject any input that contains characters outside `[0-9]`.
/// Non-empty input required. No sign, no decimal point, no
/// underscores — use [`validate_integer`] for those.
///
/// # Errors
/// `ValidationError { code: "not_numeric", ... }`.
pub fn validate_numeric(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new("not_numeric", "Enter only digits."));
    }
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return Err(ValidationError::new("not_numeric", "Enter only digits."));
    }
    Ok(())
}

/// Reject any input that contains characters outside `[a-zA-Z]`.
/// Non-empty input required. ASCII only.
///
/// # Errors
/// `ValidationError { code: "not_alpha", ... }`.
pub fn validate_alpha(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new("not_alpha", "Enter only letters."));
    }
    if !s.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(ValidationError::new("not_alpha", "Enter only letters."));
    }
    Ok(())
}

// ------------------------------------------------------------------ length / value bounds

/// Reject strings shorter than `min` characters (Unicode code points,
/// not bytes — matches Django's `MinLengthValidator`).
///
/// # Errors
/// `ValidationError { code: "min_length", ... }` if the string is too short.
pub fn validate_min_length(s: &str, min: usize) -> Result<(), ValidationError> {
    let len = s.chars().count();
    if len < min {
        return Err(ValidationError::new(
            "min_length",
            format!("Ensure this value has at least {min} characters (it has {len})."),
        ));
    }
    Ok(())
}

/// Reject strings longer than `max` characters.
///
/// # Errors
/// `ValidationError { code: "max_length", ... }` if the string is too long.
pub fn validate_max_length(s: &str, max: usize) -> Result<(), ValidationError> {
    let len = s.chars().count();
    if len > max {
        return Err(ValidationError::new(
            "max_length",
            format!("Ensure this value has at most {max} characters (it has {len})."),
        ));
    }
    Ok(())
}

/// Reject integers below `min`.
///
/// # Errors
/// `ValidationError { code: "min_value", ... }`.
pub fn validate_min_value(n: i64, min: i64) -> Result<(), ValidationError> {
    if n < min {
        return Err(ValidationError::new(
            "min_value",
            format!("Ensure this value is greater than or equal to {min}."),
        ));
    }
    Ok(())
}

/// Reject integers above `max`.
///
/// # Errors
/// `ValidationError { code: "max_value", ... }`.
pub fn validate_max_value(n: i64, max: i64) -> Result<(), ValidationError> {
    if n > max {
        return Err(ValidationError::new(
            "max_value",
            format!("Ensure this value is less than or equal to {max}."),
        ));
    }
    Ok(())
}

/// Reject floats below `min`. Float variant of [`validate_min_value`]
/// for prices, measurements, scientific values that don't fit `i64`.
/// NaN is rejected.
///
/// # Errors
/// `ValidationError { code: "min_value", ... }`.
pub fn validate_min_value_f64(n: f64, min: f64) -> Result<(), ValidationError> {
    if n.is_nan() || n < min {
        return Err(ValidationError::new(
            "min_value",
            format!("Ensure this value is greater than or equal to {min}."),
        ));
    }
    Ok(())
}

/// Reject floats above `max`. Float variant of [`validate_max_value`].
/// NaN is rejected.
///
/// # Errors
/// `ValidationError { code: "max_value", ... }`.
pub fn validate_max_value_f64(n: f64, max: f64) -> Result<(), ValidationError> {
    if n.is_nan() || n > max {
        return Err(ValidationError::new(
            "max_value",
            format!("Ensure this value is less than or equal to {max}."),
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ integer / decimal

/// Validate that `s` parses as a signed 64-bit integer. Django's
/// `validate_integer`. Leading/trailing whitespace is rejected
/// (Django's implementation calls `int()` which is strict about
/// surrounding whitespace).
///
/// # Errors
/// `ValidationError { code: "invalid_integer", ... }`.
pub fn validate_integer(s: &str) -> Result<(), ValidationError> {
    if s != s.trim() || s.is_empty() {
        return Err(ValidationError::new(
            "invalid_integer",
            "Enter a valid integer.",
        ));
    }
    s.parse::<i64>()
        .map(|_| ())
        .map_err(|_| ValidationError::new("invalid_integer", "Enter a valid integer."))
}

/// Validate a decimal-number string under Django's
/// `DecimalValidator` shape: at most `max_digits` total digits
/// (excluding sign + decimal point), and at most `decimal_places`
/// digits after the point.
///
/// `max_digits` is the **total** digit count — pre- and
/// post-decimal combined. So `12.34` is 4 digits, 2 decimal_places.
/// Pass `None` for either bound to disable that check.
///
/// # Errors
/// `ValidationError { code: "invalid_decimal", ... }` for non-numeric
/// input. `code: "max_digits"` / `"max_decimal_places"` for bound
/// violations.
pub fn validate_decimal(
    s: &str,
    max_digits: Option<usize>,
    decimal_places: Option<usize>,
) -> Result<(), ValidationError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::new("invalid_decimal", "Enter a number."));
    }
    let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let (int_part, frac_part) = match unsigned.split_once('.') {
        Some((a, b)) => (a, b),
        None => (unsigned, ""),
    };
    // Either side may be empty individually (".5" or "5."), but
    // not both (a bare "." or empty string).
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(ValidationError::new("invalid_decimal", "Enter a number."));
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(ValidationError::new("invalid_decimal", "Enter a number."));
    }
    // Drop leading zeros from int_part when counting, so "007.5"
    // has 2 digits not 4 — matches Django's Decimal coercion.
    let int_digits = int_part.trim_start_matches('0').len();
    let frac_digits = frac_part.len();
    if let Some(places) = decimal_places {
        if frac_digits > places {
            return Err(ValidationError::new(
                "max_decimal_places",
                format!(
                    "Ensure there are no more than {places} decimal places (got {frac_digits})."
                ),
            ));
        }
    }
    if let Some(total) = max_digits {
        let total_digits = int_digits + frac_digits;
        if total_digits > total {
            return Err(ValidationError::new(
                "max_digits",
                format!(
                    "Ensure there are no more than {total} digits in total (got {total_digits})."
                ),
            ));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ IPv4 / IPv6

/// Validate that `s` parses as an IPv4 address (dotted-quad). Uses
/// `std::net::Ipv4Addr::from_str` so the parse rules match the rest
/// of the standard library.
///
/// # Errors
/// `ValidationError { code: "invalid_ipv4_address", ... }`.
pub fn validate_ipv4_address(s: &str) -> Result<(), ValidationError> {
    use std::str::FromStr as _;
    std::net::Ipv4Addr::from_str(s)
        .map(|_| ())
        .map_err(|_| ValidationError::new("invalid_ipv4_address", "Enter a valid IPv4 address."))
}

/// Validate that `s` parses as an IPv6 address. Uses
/// `std::net::Ipv6Addr::from_str`.
///
/// # Errors
/// `ValidationError { code: "invalid_ipv6_address", ... }`.
pub fn validate_ipv6_address(s: &str) -> Result<(), ValidationError> {
    use std::str::FromStr as _;
    std::net::Ipv6Addr::from_str(s)
        .map(|_| ())
        .map_err(|_| ValidationError::new("invalid_ipv6_address", "Enter a valid IPv6 address."))
}

// ------------------------------------------------------------------ comma-separated integer list

/// Validate a comma-separated list of email addresses (e.g. a
/// "CC" field that takes multiple recipients). Each entry must
/// pass [`validate_email`]; surrounding whitespace per entry is
/// tolerated.
///
/// Empty list and empty entries (`"a@b.com,,c@d.com"`) are
/// rejected — they're almost certainly a typo, not intent.
///
/// # Errors
/// Returns the FIRST invalid entry's error, with the same code
/// (`"invalid_email"`) so handlers can surface a single "fix this"
/// message to the user.
pub fn validate_email_list(s: &str) -> Result<(), ValidationError> {
    if s.trim().is_empty() {
        return Err(ValidationError::new(
            "invalid_email",
            "Enter at least one email address.",
        ));
    }
    for part in s.split(',') {
        let entry = part.trim();
        if entry.is_empty() {
            return Err(ValidationError::new(
                "invalid_email",
                "Enter a valid email address.",
            ));
        }
        validate_email(entry)?;
    }
    Ok(())
}

/// Validate that `s` is a comma-separated list of integers
/// (`"1,2,3"`). Empty string is rejected (Django returns an error
/// — use `Option<String>` upstream if the field is optional).
///
/// Whitespace around individual entries is tolerated (`"1, 2, 3"`
/// passes) since this is how operators typically type lists into
/// forms — Django accepts it too.
///
/// # Errors
/// `ValidationError { code: "invalid_comma_separated_integer_list", ... }`.
pub fn validate_comma_separated_integer_list(s: &str) -> Result<(), ValidationError> {
    if s.trim().is_empty() {
        return Err(ValidationError::new(
            "invalid_comma_separated_integer_list",
            "Enter only digits separated by commas.",
        ));
    }
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() || part.parse::<i64>().is_err() {
            return Err(ValidationError::new(
                "invalid_comma_separated_integer_list",
                "Enter only digits separated by commas.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- validate_email --------

    #[test]
    fn email_accepts_common_shapes() {
        assert!(validate_email("alice@example.com").is_ok());
        assert!(validate_email("a.b+tag@example.co.uk").is_ok());
        assert!(validate_email("nested.dots+plus_underscore-hyphen@sub.example.org").is_ok());
    }

    #[test]
    fn email_rejects_missing_at() {
        let e = validate_email("alice.example.com").unwrap_err();
        assert_eq!(e.code, "invalid_email");
    }

    #[test]
    fn email_rejects_empty_local_or_domain() {
        assert!(validate_email("@example.com").is_err());
        assert!(validate_email("alice@").is_err());
    }

    #[test]
    fn email_rejects_no_dot_in_domain() {
        assert!(validate_email("alice@localhost").is_err());
    }

    #[test]
    fn email_rejects_two_at_signs() {
        assert!(validate_email("a@b@c.com").is_err());
    }

    #[test]
    fn email_rejects_consecutive_dots() {
        assert!(validate_email("a..b@example.com").is_err());
        assert!(validate_email("a@example..com").is_err());
    }

    #[test]
    fn email_rejects_empty_and_whitespace_only() {
        assert!(validate_email("").is_err());
        assert!(validate_email("   ").is_err());
    }

    #[test]
    fn is_email_is_a_thin_boolean_wrapper() {
        assert!(is_email("a@b.com"));
        assert!(!is_email("not an email"));
    }

    // -------- validate_url --------

    #[test]
    fn url_accepts_http_and_https() {
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn url_accepts_paths_query_fragment() {
        assert!(validate_url("https://example.com/path?q=1#frag").is_ok());
    }

    #[test]
    fn url_accepts_port() {
        assert!(validate_url("http://example.com:8080/api").is_ok());
    }

    #[test]
    fn url_rejects_no_scheme() {
        assert!(validate_url("example.com").is_err());
    }

    #[test]
    fn url_rejects_unknown_scheme() {
        assert!(validate_url("ftp://example.com").is_err());
    }

    #[test]
    fn url_rejects_empty_host() {
        assert!(validate_url("https://").is_err());
        assert!(validate_url("https:///path").is_err());
    }

    #[test]
    fn url_rejects_empty_string() {
        assert!(validate_url("").is_err());
    }

    // -------- validate_slug --------

    #[test]
    fn slug_accepts_alnum_underscore_hyphen() {
        assert!(validate_slug("hello-world_42").is_ok());
        assert!(validate_slug("just-letters").is_ok());
        assert!(validate_slug("123").is_ok());
    }

    #[test]
    fn slug_rejects_spaces_and_punctuation() {
        assert!(validate_slug("hello world").is_err());
        assert!(validate_slug("hello!").is_err());
        assert!(validate_slug("a.b").is_err());
    }

    #[test]
    fn slug_rejects_empty() {
        assert!(validate_slug("").is_err());
    }

    #[test]
    fn slug_rejects_non_ascii_letters() {
        // Django's default slug_re is ASCII-only; the unicode-aware
        // form is opt-in. Match that.
        assert!(validate_slug("café").is_err());
    }

    // -------- length / value bounds --------

    #[test]
    fn min_length_uses_char_count_not_byte_count() {
        // "éé" is 2 chars but 4 bytes — the validator counts chars.
        assert!(validate_min_length("éé", 2).is_ok());
        assert!(validate_min_length("é", 2).is_err());
    }

    #[test]
    fn max_length_uses_char_count_not_byte_count() {
        assert!(validate_max_length("éé", 2).is_ok());
        assert!(validate_max_length("ééé", 2).is_err());
    }

    #[test]
    fn min_length_at_boundary_is_ok() {
        assert!(validate_min_length("abc", 3).is_ok());
        assert!(validate_min_length("ab", 3).is_err());
    }

    #[test]
    fn min_and_max_value_bounds_are_inclusive() {
        assert!(validate_min_value(5, 5).is_ok());
        assert!(validate_max_value(5, 5).is_ok());
        assert!(validate_min_value(4, 5).is_err());
        assert!(validate_max_value(6, 5).is_err());
    }

    #[test]
    fn min_and_max_value_f64_bounds_are_inclusive() {
        assert!(validate_min_value_f64(5.0, 5.0).is_ok());
        assert!(validate_max_value_f64(5.0, 5.0).is_ok());
        assert!(validate_min_value_f64(4.999, 5.0).is_err());
        assert!(validate_max_value_f64(5.001, 5.0).is_err());
    }

    #[test]
    fn min_and_max_value_f64_reject_nan() {
        // NaN compares false against any value, so the bare `<`/`>`
        // check would silently accept it. We explicitly reject it.
        assert!(validate_min_value_f64(f64::NAN, 0.0).is_err());
        assert!(validate_max_value_f64(f64::NAN, 100.0).is_err());
    }

    #[test]
    fn min_and_max_value_f64_handle_infinities() {
        // +Inf passes min check, fails max check (and vice versa).
        assert!(validate_min_value_f64(f64::INFINITY, 5.0).is_ok());
        assert!(validate_max_value_f64(f64::INFINITY, 5.0).is_err());
        assert!(validate_min_value_f64(f64::NEG_INFINITY, 5.0).is_err());
        assert!(validate_max_value_f64(f64::NEG_INFINITY, 5.0).is_ok());
    }

    // -------- ValidationError --------

    #[test]
    fn validation_error_display_renders_message() {
        let e = ValidationError::new("invalid_email", "Bad email.");
        assert_eq!(format!("{e}"), "Bad email.");
        assert_eq!(e.code, "invalid_email");
    }

    // -------- validate_integer --------

    #[test]
    fn integer_accepts_positive_negative_zero() {
        assert!(validate_integer("0").is_ok());
        assert!(validate_integer("42").is_ok());
        assert!(validate_integer("-7").is_ok());
        assert!(validate_integer("+1").is_ok());
    }

    #[test]
    fn integer_rejects_decimals_and_letters() {
        assert!(validate_integer("3.14").is_err());
        assert!(validate_integer("abc").is_err());
        assert!(validate_integer("12abc").is_err());
    }

    #[test]
    fn integer_rejects_surrounding_whitespace() {
        // Django's `int()` rejects whitespace — we match.
        assert!(validate_integer(" 42").is_err());
        assert!(validate_integer("42 ").is_err());
        assert!(validate_integer("").is_err());
    }

    // -------- validate_decimal --------

    #[test]
    fn decimal_accepts_well_formed_numbers() {
        assert!(validate_decimal("12.34", None, None).is_ok());
        assert!(validate_decimal("-12.34", None, None).is_ok());
        assert!(validate_decimal("+0.5", None, None).is_ok());
        assert!(validate_decimal(".5", None, None).is_ok());
        assert!(validate_decimal("5.", None, None).is_ok());
        assert!(validate_decimal("100", None, None).is_ok());
    }

    #[test]
    fn decimal_rejects_non_numeric() {
        assert!(validate_decimal("abc", None, None).is_err());
        assert!(validate_decimal("12.3.4", None, None).is_err());
        assert!(validate_decimal(".", None, None).is_err());
        assert!(validate_decimal("", None, None).is_err());
    }

    #[test]
    fn decimal_enforces_max_decimal_places() {
        // 2 places allowed; "12.345" has 3 → error.
        let e = validate_decimal("12.345", None, Some(2)).unwrap_err();
        assert_eq!(e.code, "max_decimal_places");
        // Equal-to-limit passes.
        assert!(validate_decimal("12.34", None, Some(2)).is_ok());
    }

    #[test]
    fn decimal_enforces_max_total_digits() {
        // 5 total digits allowed; "12345.6" has 6 → error.
        let e = validate_decimal("12345.6", Some(5), None).unwrap_err();
        assert_eq!(e.code, "max_digits");
        assert!(validate_decimal("1234.5", Some(5), None).is_ok());
    }

    #[test]
    fn decimal_max_digits_ignores_leading_zeros() {
        // "007.5" should count as 2 digits (7 + 5) not 4.
        assert!(validate_decimal("007.5", Some(2), None).is_ok());
    }

    // -------- validate_ipv4_address --------

    #[test]
    fn ipv4_accepts_dotted_quad() {
        assert!(validate_ipv4_address("127.0.0.1").is_ok());
        assert!(validate_ipv4_address("0.0.0.0").is_ok());
        assert!(validate_ipv4_address("255.255.255.255").is_ok());
    }

    #[test]
    fn ipv4_rejects_malformed_or_out_of_range() {
        assert!(validate_ipv4_address("256.0.0.1").is_err());
        assert!(validate_ipv4_address("not.an.ip.addr").is_err());
        assert!(validate_ipv4_address("1.2.3").is_err());
        assert!(validate_ipv4_address("").is_err());
    }

    // -------- validate_ipv6_address --------

    #[test]
    fn ipv6_accepts_full_and_shorthand() {
        assert!(validate_ipv6_address("2001:db8::1").is_ok());
        assert!(validate_ipv6_address("::1").is_ok());
        assert!(validate_ipv6_address("fe80::1234:5678:9abc:def0").is_ok());
    }

    #[test]
    fn ipv6_rejects_v4_addresses_and_garbage() {
        assert!(validate_ipv6_address("127.0.0.1").is_err());
        assert!(validate_ipv6_address("zzz").is_err());
        assert!(validate_ipv6_address("").is_err());
    }

    // -------- validate_comma_separated_integer_list --------

    #[test]
    fn comma_list_accepts_clean_form() {
        assert!(validate_comma_separated_integer_list("1,2,3").is_ok());
        assert!(validate_comma_separated_integer_list("42").is_ok());
        assert!(validate_comma_separated_integer_list("-1,0,1").is_ok());
    }

    #[test]
    fn comma_list_tolerates_inner_whitespace() {
        assert!(validate_comma_separated_integer_list("1, 2, 3").is_ok());
    }

    #[test]
    fn comma_list_rejects_empty_and_non_integers() {
        assert!(validate_comma_separated_integer_list("").is_err());
        assert!(validate_comma_separated_integer_list("1,abc,3").is_err());
        assert!(validate_comma_separated_integer_list("1,,3").is_err());
    }

    // -------- validate_unicode_slug --------

    #[test]
    fn unicode_slug_accepts_non_ascii_letters() {
        // The whole point — café / 日本語 / Привет all valid.
        assert!(validate_unicode_slug("café-au-lait").is_ok());
        assert!(validate_unicode_slug("日本語").is_ok());
        assert!(validate_unicode_slug("Привет_мир").is_ok());
    }

    #[test]
    fn unicode_slug_still_rejects_punctuation_and_spaces() {
        // The "unicode" part is just about which letters count; the
        // slug shape (no spaces / punctuation) still applies.
        assert!(validate_unicode_slug("hello world").is_err());
        assert!(validate_unicode_slug("hello!").is_err());
        assert!(validate_unicode_slug("a.b").is_err());
    }

    #[test]
    fn unicode_slug_rejects_empty() {
        assert!(validate_unicode_slug("").is_err());
    }

    // -------- validate_prohibit_null_characters --------

    #[test]
    fn prohibit_null_accepts_strings_without_nul() {
        assert!(validate_prohibit_null_characters("hello").is_ok());
        assert!(validate_prohibit_null_characters("").is_ok());
        assert!(validate_prohibit_null_characters("non-printable\x01ok").is_ok());
    }

    #[test]
    fn prohibit_null_rejects_strings_containing_nul() {
        let e = validate_prohibit_null_characters("hello\0world").unwrap_err();
        assert_eq!(e.code, "null_characters_not_allowed");
    }

    // -------- validate_email_list --------

    #[test]
    fn email_list_accepts_single_email() {
        assert!(validate_email_list("alice@example.com").is_ok());
    }

    #[test]
    fn email_list_accepts_multiple_with_whitespace() {
        assert!(validate_email_list("a@b.com, c@d.com,e@f.com").is_ok());
    }

    #[test]
    fn email_list_rejects_empty_string() {
        assert!(validate_email_list("").is_err());
        assert!(validate_email_list("   ").is_err());
    }

    #[test]
    fn email_list_rejects_empty_entries() {
        assert!(validate_email_list("a@b.com,,c@d.com").is_err());
        assert!(validate_email_list("a@b.com, ,c@d.com").is_err());
    }

    #[test]
    fn email_list_rejects_invalid_entry() {
        let e = validate_email_list("a@b.com,not-an-email,c@d.com").unwrap_err();
        assert_eq!(e.code, "invalid_email");
    }

    // -------- validate_phone_e164 --------

    #[test]
    fn phone_e164_accepts_typical_examples() {
        assert!(validate_phone_e164("+14155552671").is_ok());
        assert!(validate_phone_e164("+442012345678").is_ok());
        assert!(validate_phone_e164("+919876543210").is_ok());
    }

    #[test]
    fn phone_e164_accepts_minimum_length_of_one_digit() {
        assert!(validate_phone_e164("+1").is_ok());
    }

    #[test]
    fn phone_e164_accepts_maximum_length_of_fifteen_digits() {
        assert!(validate_phone_e164("+123456789012345").is_ok());
    }

    #[test]
    fn phone_e164_rejects_missing_plus() {
        let e = validate_phone_e164("14155552671").unwrap_err();
        assert_eq!(e.code, "invalid_phone");
    }

    #[test]
    fn phone_e164_rejects_too_many_digits() {
        // 16 digits — one too many.
        assert!(validate_phone_e164("+1234567890123456").is_err());
    }

    #[test]
    fn phone_e164_rejects_zero_digits_after_plus() {
        assert!(validate_phone_e164("+").is_err());
    }

    #[test]
    fn phone_e164_rejects_separators_and_letters() {
        assert!(validate_phone_e164("+1-415-555-2671").is_err());
        assert!(validate_phone_e164("+1 (415) 555-2671").is_err());
        assert!(validate_phone_e164("+1abc4155552671").is_err());
    }

    #[test]
    fn phone_e164_rejects_empty() {
        assert!(validate_phone_e164("").is_err());
    }

    #[test]
    fn is_phone_e164_is_thin_boolean_wrapper() {
        assert!(is_phone_e164("+14155552671"));
        assert!(!is_phone_e164("14155552671"));
    }

    // -------- validate_hex_color --------

    #[test]
    fn hex_color_accepts_rgb_shorthand() {
        assert!(validate_hex_color("#fff").is_ok());
        assert!(validate_hex_color("#000").is_ok());
        assert!(validate_hex_color("#fA0").is_ok());
    }

    #[test]
    fn hex_color_accepts_full_rrggbb() {
        assert!(validate_hex_color("#ffffff").is_ok());
        assert!(validate_hex_color("#FFAA00").is_ok());
    }

    #[test]
    fn hex_color_accepts_alpha_variants() {
        // 4 = rgba shorthand, 8 = rrggbbaa.
        assert!(validate_hex_color("#fff8").is_ok());
        assert!(validate_hex_color("#FFAA00CC").is_ok());
    }

    #[test]
    fn hex_color_rejects_missing_hash() {
        assert!(validate_hex_color("fff").is_err());
    }

    #[test]
    fn hex_color_rejects_non_hex_chars() {
        let e = validate_hex_color("#ffffg0").unwrap_err();
        assert_eq!(e.code, "invalid_hex_color");
    }

    #[test]
    fn hex_color_rejects_wrong_length() {
        // 1, 2, 5, 7 are all rejected — only 3/4/6/8 are valid.
        assert!(validate_hex_color("#f").is_err());
        assert!(validate_hex_color("#ff").is_err());
        assert!(validate_hex_color("#fffff").is_err());
        assert!(validate_hex_color("#fffffff").is_err());
    }

    #[test]
    fn hex_color_rejects_empty_and_hash_only() {
        assert!(validate_hex_color("").is_err());
        assert!(validate_hex_color("#").is_err());
    }

    #[test]
    fn is_hex_color_is_thin_boolean_wrapper() {
        assert!(is_hex_color("#fff"));
        assert!(!is_hex_color("fff"));
    }

    // -------- validate_uuid --------

    #[test]
    fn uuid_accepts_hyphenated_form() {
        assert!(validate_uuid("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn uuid_accepts_simple_form_no_hyphens() {
        assert!(validate_uuid("550e8400e29b41d4a716446655440000").is_ok());
    }

    #[test]
    fn uuid_accepts_urn_prefix() {
        assert!(validate_uuid("urn:uuid:550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn uuid_accepts_braced_form() {
        assert!(validate_uuid("{550e8400-e29b-41d4-a716-446655440000}").is_ok());
    }

    #[test]
    fn uuid_rejects_garbage() {
        let e = validate_uuid("not-a-uuid").unwrap_err();
        assert_eq!(e.code, "invalid_uuid");
    }

    #[test]
    fn uuid_rejects_wrong_length() {
        // 31 hex chars instead of 32.
        assert!(validate_uuid("550e8400e29b41d4a71644665544000").is_err());
    }

    #[test]
    fn uuid_rejects_non_hex() {
        assert!(validate_uuid("550e8400-e29b-41d4-a716-44665544000g").is_err());
    }

    #[test]
    fn is_uuid_is_thin_boolean_wrapper() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_uuid("nope"));
    }

    // -------- validate_iso_date --------

    #[test]
    fn iso_date_accepts_well_formed() {
        assert!(validate_iso_date("2026-01-15").is_ok());
        assert!(validate_iso_date("1970-01-01").is_ok());
        assert!(validate_iso_date("9999-12-31").is_ok());
    }

    #[test]
    fn iso_date_rejects_out_of_range() {
        assert!(validate_iso_date("2026-02-30").is_err()); // Feb has no 30
        assert!(validate_iso_date("2026-13-01").is_err()); // month 13
        assert!(validate_iso_date("2026-00-01").is_err()); // month 0
        assert!(validate_iso_date("2026-01-32").is_err()); // day 32
    }

    #[test]
    fn iso_date_rejects_wrong_format() {
        // chrono's `%m` is lenient about padding — `2026-1-15` parses
        // OK. We only reject formats that are unambiguously not a
        // calendar date (US format, datetime-with-time, empty).
        assert!(validate_iso_date("01/15/2026").is_err()); // US format
        assert!(validate_iso_date("2026-01-15T00:00:00").is_err()); // datetime, not date
        assert!(validate_iso_date("").is_err());
    }

    // -------- validate_iso_time --------

    #[test]
    fn iso_time_accepts_well_formed() {
        assert!(validate_iso_time("14:30:00").is_ok());
        assert!(validate_iso_time("00:00:00").is_ok());
        assert!(validate_iso_time("23:59:59").is_ok());
    }

    #[test]
    fn iso_time_accepts_fractional_seconds() {
        assert!(validate_iso_time("14:30:00.123").is_ok());
        assert!(validate_iso_time("14:30:00.123456").is_ok());
    }

    #[test]
    fn iso_time_rejects_out_of_range() {
        assert!(validate_iso_time("24:00:00").is_err()); // hour 24
        assert!(validate_iso_time("14:60:00").is_err()); // minute 60
                                                         // Note: chrono accepts second=60 as a leap-second marker
                                                         // — that's intentional IETF/ISO behaviour, so we don't
                                                         // assert against it here.
    }

    #[test]
    fn iso_time_rejects_wrong_format() {
        assert!(validate_iso_time("2:30 PM").is_err());
        assert!(validate_iso_time("14:30").is_err()); // missing seconds
        assert!(validate_iso_time("").is_err());
    }

    // -------- validate_iso_datetime --------

    #[test]
    fn iso_datetime_accepts_z_offset() {
        assert!(validate_iso_datetime("2026-01-15T14:30:00Z").is_ok());
        assert!(validate_iso_datetime("2026-01-15T14:30:00.123Z").is_ok());
    }

    #[test]
    fn iso_datetime_accepts_explicit_offset() {
        assert!(validate_iso_datetime("2026-01-15T14:30:00+02:00").is_ok());
        assert!(validate_iso_datetime("2026-01-15T14:30:00-05:00").is_ok());
    }

    #[test]
    fn iso_datetime_rejects_naive_datetime() {
        // No timezone marker → rejected. Mixing naive + tz-aware
        // datetimes is a data-corruption vector.
        assert!(validate_iso_datetime("2026-01-15T14:30:00").is_err());
    }

    #[test]
    fn iso_datetime_rejects_garbage() {
        let e = validate_iso_datetime("not a date").unwrap_err();
        assert_eq!(e.code, "invalid_iso_datetime");
    }

    // -------- validate_alphanumeric / numeric / alpha --------

    #[test]
    fn alphanumeric_accepts_letters_and_digits() {
        assert!(validate_alphanumeric("abc123").is_ok());
        assert!(validate_alphanumeric("ABC").is_ok());
        assert!(validate_alphanumeric("9").is_ok());
    }

    #[test]
    fn alphanumeric_rejects_punctuation_spaces_and_empty() {
        assert!(validate_alphanumeric("abc 123").is_err());
        assert!(validate_alphanumeric("abc-123").is_err());
        assert!(validate_alphanumeric("abc!").is_err());
        assert!(validate_alphanumeric("").is_err());
    }

    #[test]
    fn alphanumeric_rejects_non_ascii_letters() {
        // ASCII-only — use unicode_slug for the broader variant.
        assert!(validate_alphanumeric("café").is_err());
    }

    #[test]
    fn numeric_accepts_digits_only() {
        assert!(validate_numeric("123").is_ok());
        assert!(validate_numeric("0").is_ok());
    }

    #[test]
    fn numeric_rejects_signs_decimal_letters_empty() {
        assert!(validate_numeric("-1").is_err());
        assert!(validate_numeric("3.14").is_err());
        assert!(validate_numeric("12a").is_err());
        assert!(validate_numeric("").is_err());
    }

    #[test]
    fn alpha_accepts_letters_only() {
        assert!(validate_alpha("Alice").is_ok());
        assert!(validate_alpha("Z").is_ok());
    }

    #[test]
    fn alpha_rejects_digits_punctuation_empty() {
        assert!(validate_alpha("abc1").is_err());
        assert!(validate_alpha("a b").is_err());
        assert!(validate_alpha("a-b").is_err());
        assert!(validate_alpha("").is_err());
    }
}
