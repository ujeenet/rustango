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
//! - `validate_creditcard_luhn` — Luhn checksum on a credit card
//!   PAN string. Catches typos before hitting the PCI processor;
//!   does NOT verify the card is real / not expired / has funds.
//! - `validate_isbn` — ISBN-10 or ISBN-13 checksum (auto-detects
//!   which form by digit count).
//! - `validate_hostname` — RFC 1123 hostname format. Each
//!   dot-separated label 1–63 chars, letters/digits/hyphens, no
//!   leading or trailing hyphen, total length ≤ 253.
//! - `validate_iban` — ISO 13616 IBAN mod-97 checksum (catches typos
//!   in bank account fields).
//! - `validate_mac_address` — EUI-48 MAC address (6 hex pairs
//!   separated by `:` or `-`).
//! - `validate_base64` — standard base64 (`[A-Za-z0-9+/]` +
//!   optional `=` padding).
//! - `validate_base64_urlsafe` — URL-safe base64 (`[A-Za-z0-9_-]`).
//! - `validate_jwt_shape` — three dot-separated URL-safe base64
//!   segments. Shape check only, NO signature verification.
//! - `validate_semver` — semantic version 2.0.0 (`MAJOR.MINOR.PATCH`
//!   with optional `-pre.release` and `+build` suffixes).
//! - `validate_country_code` — ISO 3166-1 alpha-2 country code
//!   (2 uppercase letters). Format-only; doesn't check the code
//!   exists.
//! - `validate_currency_code` — ISO 4217 currency code (3 uppercase
//!   letters). Format-only.
//! - `validate_language_tag` — BCP 47 light: `lang[-region]`
//!   (e.g. `en`, `en-US`, `fr-CA`, `zh-Hans`).
//! - `validate_postal_code_us` — US ZIP code in `12345` or
//!   `12345-6789` (ZIP+4) form.
//! - `validate_postal_code_ca` — Canadian postal code in `A1A 1A1`
//!   form (uppercase, single space).
//! - `validate_postal_code_uk` — UK postcode (`SW1A 1AA` shape;
//!   loose check on the well-formed cases).
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

// ------------------------------------------------------------------ credit card (Luhn)

/// Verify the Luhn checksum on a credit-card-shape Primary Account
/// Number (PAN). Strips spaces and hyphens (the typical
/// human-typed shape), then checks that:
///
/// 1. Every remaining character is a digit.
/// 2. The total length is between 12 and 19 (current PAN range).
/// 3. The Luhn algorithm reports a valid trailing check digit.
///
/// **Scope**: catches typos before hitting the PCI processor. This
/// does NOT verify the card is real / unexpired / has funds — only
/// the upstream payment-gateway authorization can do that. Use as
/// a client-side sanity check, never as the only line of defence.
///
/// # Errors
/// `ValidationError { code: "invalid_card_number", ... }`.
pub fn validate_creditcard_luhn(s: &str) -> Result<(), ValidationError> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return Err(ValidationError::new(
            "invalid_card_number",
            "Enter a valid credit card number.",
        ));
    }
    if !(12..=19).contains(&cleaned.len()) {
        return Err(ValidationError::new(
            "invalid_card_number",
            "Enter a valid credit card number.",
        ));
    }
    // Luhn: walk digits right-to-left. Every second digit (starting
    // at the second from the right) is doubled; if the doubled value
    // is >= 10, sum its digits (which is equivalent to subtracting 9).
    // Total must be divisible by 10.
    let mut sum = 0u32;
    let mut double = false;
    for ch in cleaned.chars().rev() {
        let mut d = ch.to_digit(10).expect("digit-only by check above");
        if double {
            d *= 2;
            if d >= 10 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    if sum % 10 != 0 {
        return Err(ValidationError::new(
            "invalid_card_number",
            "Enter a valid credit card number.",
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ ISBN

/// Verify the checksum on an ISBN-10 or ISBN-13 string. Strips
/// spaces and hyphens (the typical printed shape), then dispatches
/// on digit count:
///
/// - **10 chars**: ISBN-10 form. First 9 must be digits; 10th may
///   be `0`–`9` or `X` (representing 10). Checksum:
///   `sum(i * d_i) mod 11 == 0` for i = 1..10.
/// - **13 chars**: ISBN-13 form. All digits. Checksum:
///   `sum(d_i * w_i) mod 10 == 0` where weights alternate
///   `1, 3, 1, 3, …`.
///
/// Anything else is rejected. Used by Library / catalog admin
/// pages to catch typoed ISBNs at form-input time before the row
/// hits the DB.
///
/// # Errors
/// `ValidationError { code: "invalid_isbn", ... }`.
pub fn validate_isbn(s: &str) -> Result<(), ValidationError> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    match cleaned.len() {
        10 => validate_isbn_10(&cleaned),
        13 => validate_isbn_13(&cleaned),
        _ => Err(ValidationError::new(
            "invalid_isbn",
            "Enter a valid ISBN-10 or ISBN-13.",
        )),
    }
}

fn validate_isbn_10(s: &str) -> Result<(), ValidationError> {
    // First 9 chars must be digits; last may be `X` for 10.
    let chars: Vec<char> = s.chars().collect();
    let mut sum = 0u32;
    for (i, &ch) in chars.iter().enumerate() {
        let digit = if i == 9 && (ch == 'X' || ch == 'x') {
            10
        } else if let Some(d) = ch.to_digit(10) {
            d
        } else {
            return Err(ValidationError::new(
                "invalid_isbn",
                "Enter a valid ISBN-10 or ISBN-13.",
            ));
        };
        // Weight = (i + 1) per the ISBN-10 spec.
        sum += digit * (u32::try_from(i).unwrap_or(0) + 1);
    }
    if sum % 11 != 0 {
        return Err(ValidationError::new(
            "invalid_isbn",
            "Enter a valid ISBN-10 or ISBN-13.",
        ));
    }
    Ok(())
}

fn validate_isbn_13(s: &str) -> Result<(), ValidationError> {
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return Err(ValidationError::new(
            "invalid_isbn",
            "Enter a valid ISBN-10 or ISBN-13.",
        ));
    }
    let mut sum = 0u32;
    for (i, ch) in s.chars().enumerate() {
        let d = ch.to_digit(10).expect("digit-only by check above");
        // Alternating weights: 1, 3, 1, 3, ...
        let weight = if i.is_multiple_of(2) { 1 } else { 3 };
        sum += d * weight;
    }
    if sum % 10 != 0 {
        return Err(ValidationError::new(
            "invalid_isbn",
            "Enter a valid ISBN-10 or ISBN-13.",
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ hostname

/// Validate an RFC 1123 hostname: dot-separated labels, each label
/// 1–63 chars containing only ASCII letters / digits / hyphens, no
/// leading or trailing hyphen on any label, total length ≤ 253.
/// Empty string is rejected.
///
/// Suited for admin form fields that take a server hostname /
/// DNS name. Distinct from `validate_url` (which expects a scheme)
/// and `validate_ipv4_address` (which is for literal IPs).
///
/// Examples:
/// - `validate_hostname("example.com")` → Ok
/// - `validate_hostname("sub.example.co.uk")` → Ok
/// - `validate_hostname("localhost")` → Ok (single-label allowed)
/// - `validate_hostname("-bad.example.com")` → Err (leading `-`)
/// - `validate_hostname("very-long-label-that-exceeds-the-sixty-three-character-limit-XX.com")` → Err
/// - `validate_hostname("")` → Err
///
/// # Errors
/// `ValidationError { code: "invalid_hostname", ... }`.
pub fn validate_hostname(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() || s.len() > 253 {
        return Err(ValidationError::new(
            "invalid_hostname",
            "Enter a valid hostname.",
        ));
    }
    // Reject leading/trailing dot at the whole-name level.
    if s.starts_with('.') || s.ends_with('.') {
        return Err(ValidationError::new(
            "invalid_hostname",
            "Enter a valid hostname.",
        ));
    }
    for label in s.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(ValidationError::new(
                "invalid_hostname",
                "Enter a valid hostname.",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ValidationError::new(
                "invalid_hostname",
                "Enter a valid hostname.",
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(ValidationError::new(
                "invalid_hostname",
                "Enter a valid hostname.",
            ));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ IBAN

/// Validate an ISO 13616 IBAN (International Bank Account Number)
/// via the mod-97 check. Strips spaces (the typical printed
/// "GB82 WEST 1234 5698 7654 32" shape) before validating.
///
/// Format requirements:
/// - 2-letter ISO country code (uppercase).
/// - 2-digit check digits.
/// - 1–30 additional ASCII alphanumeric chars.
/// - Total length 5–34.
///
/// Algorithm:
/// 1. Move the first 4 chars to the end.
/// 2. Replace each letter with two digits: A=10, B=11, ..., Z=35.
/// 3. Treat the result as a single integer; mod 97 must equal 1.
///
/// This catches typos before hitting the bank-rails verification
/// API. Does NOT verify the account exists / has funds — only the
/// payment provider can do that.
///
/// # Errors
/// `ValidationError { code: "invalid_iban", ... }`.
pub fn validate_iban(s: &str) -> Result<(), ValidationError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !(5..=34).contains(&cleaned.len()) {
        return Err(ValidationError::new("invalid_iban", "Enter a valid IBAN."));
    }
    let bytes = cleaned.as_bytes();
    // First 2 must be uppercase letters (country code), next 2 digits.
    if !bytes[0].is_ascii_uppercase()
        || !bytes[1].is_ascii_uppercase()
        || !bytes[2].is_ascii_digit()
        || !bytes[3].is_ascii_digit()
    {
        return Err(ValidationError::new("invalid_iban", "Enter a valid IBAN."));
    }
    // Remaining chars must be uppercase alphanumeric.
    if !bytes[4..]
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Err(ValidationError::new("invalid_iban", "Enter a valid IBAN."));
    }
    // Rearrange: move first 4 to end. Then convert letters → digits.
    let rearranged: String = cleaned[4..]
        .chars()
        .chain(cleaned[..4].chars())
        .flat_map(|c| {
            if c.is_ascii_digit() {
                vec![c]
            } else {
                let n = c as u32 - 'A' as u32 + 10;
                n.to_string().chars().collect()
            }
        })
        .collect();
    // mod-97 on a string of digits — fold from the left to avoid
    // overflowing u64 on the full 30+-digit number.
    let mut remainder: u64 = 0;
    for ch in rearranged.chars() {
        let d = ch.to_digit(10).expect("digit-only after letter map");
        remainder = (remainder * 10 + u64::from(d)) % 97;
    }
    if remainder != 1 {
        return Err(ValidationError::new("invalid_iban", "Enter a valid IBAN."));
    }
    Ok(())
}

// ------------------------------------------------------------------ MAC address

/// Validate an EUI-48 MAC address: six pairs of hex digits
/// separated by `:` or `-`. Case-insensitive. The separator must
/// be consistent across the string (no mix-and-match between `:`
/// and `-`).
///
/// Examples:
/// - `validate_mac_address("00:1A:2B:3C:4D:5E")` → Ok
/// - `validate_mac_address("00-1a-2b-3c-4d-5e")` → Ok (case-insensitive)
/// - `validate_mac_address("001A2B3C4D5E")` → Err (separators required)
/// - `validate_mac_address("00:1A:2B-3C:4D:5E")` → Err (mixed separators)
///
/// # Errors
/// `ValidationError { code: "invalid_mac_address", ... }`.
pub fn validate_mac_address(s: &str) -> Result<(), ValidationError> {
    // 6 pairs of hex digits + 5 separators = 17 chars exactly.
    if s.len() != 17 {
        return Err(ValidationError::new(
            "invalid_mac_address",
            "Enter a valid MAC address (e.g. 00:1A:2B:3C:4D:5E).",
        ));
    }
    // Detect the separator from position 2.
    let sep = s.as_bytes()[2] as char;
    if sep != ':' && sep != '-' {
        return Err(ValidationError::new(
            "invalid_mac_address",
            "Enter a valid MAC address (e.g. 00:1A:2B:3C:4D:5E).",
        ));
    }
    let parts: Vec<&str> = s.split(sep).collect();
    if parts.len() != 6 {
        return Err(ValidationError::new(
            "invalid_mac_address",
            "Enter a valid MAC address (e.g. 00:1A:2B:3C:4D:5E).",
        ));
    }
    for part in parts {
        if part.len() != 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ValidationError::new(
                "invalid_mac_address",
                "Enter a valid MAC address (e.g. 00:1A:2B:3C:4D:5E).",
            ));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ base64

/// Validate that `s` is a well-formed standard base64 string:
/// characters from `[A-Za-z0-9+/]`, length a multiple of 4 after
/// optional `=` padding (at most two trailing `=`).
///
/// Doesn't decode the bytes — just checks the shape. Useful for
/// admin form fields capturing encoded secrets / tokens / blobs
/// where the caller will decode separately.
///
/// # Errors
/// `ValidationError { code: "invalid_base64", ... }`.
pub fn validate_base64(s: &str) -> Result<(), ValidationError> {
    validate_base64_impl(s, false)
}

/// Validate that `s` is a well-formed URL-safe base64 string:
/// characters from `[A-Za-z0-9_-]`, length a multiple of 4 after
/// optional `=` padding (or, by RFC 4648 convention, padding may
/// be omitted in URL-safe encoding — we accept either).
///
/// # Errors
/// `ValidationError { code: "invalid_base64", ... }`.
pub fn validate_base64_urlsafe(s: &str) -> Result<(), ValidationError> {
    validate_base64_impl(s, true)
}

fn validate_base64_impl(s: &str, urlsafe: bool) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "invalid_base64",
            "Enter a valid base64 string.",
        ));
    }
    // Trailing padding count.
    let pad = s.bytes().rev().take_while(|b| *b == b'=').count();
    if pad > 2 {
        return Err(ValidationError::new(
            "invalid_base64",
            "Enter a valid base64 string.",
        ));
    }
    let body = &s[..s.len() - pad];
    // Body must contain no `=`. Body chars must be in the right
    // alphabet.
    for ch in body.chars() {
        let ok = ch.is_ascii_alphanumeric()
            || (if urlsafe {
                ch == '-' || ch == '_'
            } else {
                ch == '+' || ch == '/'
            });
        if !ok {
            return Err(ValidationError::new(
                "invalid_base64",
                "Enter a valid base64 string.",
            ));
        }
    }
    // Standard base64 with padding: total length multiple of 4.
    // URL-safe base64 without padding: any length is fine PROVIDED
    // there's no padding present. URL-safe WITH padding follows the
    // standard rule.
    if (pad > 0 || !urlsafe) && !s.len().is_multiple_of(4) {
        return Err(ValidationError::new(
            "invalid_base64",
            "Enter a valid base64 string.",
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ JWT shape

/// Validate that `s` looks like a JWT: three URL-safe base64 segments
/// separated by `.`, each non-empty.
///
/// **Shape check only — does NOT verify the signature.** The whole
/// point of this validator is to catch a typoed / truncated JWT at
/// form-input time before the real JWT library returns a less clear
/// error. Use [`crate::auth`] / `jsonwebtoken` to actually verify
/// the signature and claims.
///
/// # Errors
/// `ValidationError { code: "invalid_jwt", ... }`.
pub fn validate_jwt_shape(s: &str) -> Result<(), ValidationError> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return Err(ValidationError::new(
            "invalid_jwt",
            "Enter a valid JWT (header.payload.signature).",
        ));
    }
    for part in &parts {
        if part.is_empty() {
            return Err(ValidationError::new(
                "invalid_jwt",
                "Enter a valid JWT (header.payload.signature).",
            ));
        }
        // Each part is unpadded URL-safe base64.
        validate_base64_urlsafe(part).map_err(|_| {
            ValidationError::new(
                "invalid_jwt",
                "Enter a valid JWT (header.payload.signature).",
            )
        })?;
    }
    Ok(())
}

// ------------------------------------------------------------------ semver

/// Validate a [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html)
/// version string: `MAJOR.MINOR.PATCH` with optional `-pre.release`
/// and `+build.metadata` suffixes.
///
/// Rules:
/// - MAJOR / MINOR / PATCH are non-negative integers. Leading zeros
///   are forbidden except the bare `0` itself.
/// - Pre-release: dot-separated identifiers, each non-empty,
///   `[0-9A-Za-z-]`. Numeric identifiers must not have leading
///   zeros (except `0`).
/// - Build metadata: dot-separated identifiers, each non-empty,
///   `[0-9A-Za-z-]`. Numeric leading zeros ARE allowed (per spec).
///
/// Examples:
/// - `validate_semver("1.0.0")` → Ok
/// - `validate_semver("1.0.0-alpha.1")` → Ok
/// - `validate_semver("1.0.0+20240101")` → Ok
/// - `validate_semver("1.0.0-rc.1+build.42")` → Ok
/// - `validate_semver("1.0")` → Err (missing patch)
/// - `validate_semver("01.0.0")` → Err (leading zero in major)
/// - `validate_semver("1.0.0-")` → Err (empty pre-release)
///
/// # Errors
/// `ValidationError { code: "invalid_semver", ... }`.
pub fn validate_semver(s: &str) -> Result<(), ValidationError> {
    let bad = || ValidationError::new("invalid_semver", "Enter a valid semver (e.g. 1.2.3).");
    // Split off build metadata (after first `+`).
    let (core_pre, build) = match s.split_once('+') {
        Some((cp, b)) => (cp, Some(b)),
        None => (s, None),
    };
    // Split off pre-release (after first `-`).
    let (core, pre) = match core_pre.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (core_pre, None),
    };
    // Core: exactly three dot-separated non-negative-integer
    // identifiers with no leading zeros.
    let core_parts: Vec<&str> = core.split('.').collect();
    if core_parts.len() != 3 {
        return Err(bad());
    }
    for part in core_parts {
        if !is_valid_numeric_id(part) {
            return Err(bad());
        }
    }
    if let Some(p) = pre {
        if !is_valid_semver_id_list(p, /* allow_numeric_leading_zero = */ false) {
            return Err(bad());
        }
    }
    if let Some(b) = build {
        // Build metadata identifiers may have leading zeros.
        if !is_valid_semver_id_list(b, /* allow_numeric_leading_zero = */ true) {
            return Err(bad());
        }
    }
    Ok(())
}

fn is_valid_numeric_id(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // No leading zero unless the value IS "0".
    !(s.len() > 1 && s.starts_with('0'))
}

fn is_valid_semver_id_list(s: &str, allow_numeric_leading_zero: bool) -> bool {
    if s.is_empty() {
        return false;
    }
    for part in s.split('.') {
        if part.is_empty() {
            return false;
        }
        if !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return false;
        }
        // Numeric-only identifier without leading-zero permission?
        if !allow_numeric_leading_zero
            && part.chars().all(|c| c.is_ascii_digit())
            && part.len() > 1
            && part.starts_with('0')
        {
            return false;
        }
    }
    true
}

// ------------------------------------------------------------------ ISO country / currency codes

/// Validate an ISO 3166-1 alpha-2 country code: exactly 2 uppercase
/// ASCII letters (`US`, `GB`, `DE`, `JP`, ...). Format-only — does
/// NOT check that the code corresponds to a real country (that
/// would need an embedded list of 249 codes maintained against
/// every ISO update).
///
/// # Errors
/// `ValidationError { code: "invalid_country_code", ... }`.
pub fn validate_country_code(s: &str) -> Result<(), ValidationError> {
    if s.len() != 2 || !s.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(ValidationError::new(
            "invalid_country_code",
            "Enter a 2-letter ISO 3166-1 country code (e.g. US, GB).",
        ));
    }
    Ok(())
}

/// Validate an ISO 4217 currency code: exactly 3 uppercase ASCII
/// letters (`USD`, `EUR`, `GBP`, `JPY`, ...). Format-only — does
/// NOT check that the code corresponds to a circulating currency.
///
/// # Errors
/// `ValidationError { code: "invalid_currency_code", ... }`.
pub fn validate_currency_code(s: &str) -> Result<(), ValidationError> {
    if s.len() != 3 || !s.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(ValidationError::new(
            "invalid_currency_code",
            "Enter a 3-letter ISO 4217 currency code (e.g. USD, EUR).",
        ));
    }
    Ok(())
}

/// Validate a [BCP 47](https://tools.ietf.org/html/bcp47)-shape
/// language tag in its most common forms:
/// - `lang` — 2 or 3 lowercase letters (ISO 639-1 / 639-2).
/// - `lang-REGION` — 2-letter region (`en-US`, `fr-CA`).
/// - `lang-Script` — 4-letter Title-case script subtag
///   (`zh-Hans`, `sr-Cyrl`).
/// - `lang-Script-REGION` — both (`zh-Hans-CN`).
/// - Numeric 3-digit UN region code in place of the 2-letter
///   region (`es-419` for Latin America).
///
/// **Subset only** — doesn't cover the full BCP 47 grammar (no
/// extensions, no private use, no variants beyond Script).
/// Doesn't validate that the language / region codes correspond
/// to real entries in the IANA registry — that needs an embedded
/// list.
///
/// Examples:
/// - `validate_language_tag("en")` → Ok
/// - `validate_language_tag("en-US")` → Ok
/// - `validate_language_tag("fr-CA")` → Ok
/// - `validate_language_tag("zh-Hans-CN")` → Ok
/// - `validate_language_tag("es-419")` → Ok
/// - `validate_language_tag("EN")` → Err (uppercase lang)
/// - `validate_language_tag("en-us")` → Err (lowercase region)
/// - `validate_language_tag("english")` → Err (too long)
///
/// # Errors
/// `ValidationError { code: "invalid_language_tag", ... }`.
pub fn validate_language_tag(s: &str) -> Result<(), ValidationError> {
    let bad = || {
        ValidationError::new(
            "invalid_language_tag",
            "Enter a valid language tag (e.g. en, en-US, zh-Hans-CN).",
        )
    };
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(bad());
    }
    // Part 0: language subtag — 2 or 3 lowercase letters.
    let lang = parts[0];
    if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_lowercase()) {
        return Err(bad());
    }
    let mut idx = 1;
    // Optional script subtag — 4 letters, Title-case (Xxxx).
    if idx < parts.len() {
        let p = parts[idx];
        if p.len() == 4 && is_script_subtag(p) {
            idx += 1;
        }
    }
    // Optional region subtag — 2 uppercase letters OR 3 digits.
    if idx < parts.len() {
        let p = parts[idx];
        let is_alpha2 = p.len() == 2 && p.chars().all(|c| c.is_ascii_uppercase());
        let is_num3 = p.len() == 3 && p.chars().all(|c| c.is_ascii_digit());
        if !is_alpha2 && !is_num3 {
            return Err(bad());
        }
        idx += 1;
    }
    if idx != parts.len() {
        return Err(bad());
    }
    Ok(())
}

fn is_script_subtag(s: &str) -> bool {
    if s.len() != 4 {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase())
}

// ------------------------------------------------------------------ postal code (US)

/// Validate a US ZIP code: either 5 digits (`94110`) or the
/// ZIP+4 form `12345-6789`. No other separators or formats are
/// accepted.
///
/// Examples:
/// - `validate_postal_code_us("94110")` → Ok
/// - `validate_postal_code_us("12345-6789")` → Ok
/// - `validate_postal_code_us("123456789")` → Err (need hyphen for +4)
/// - `validate_postal_code_us("9411")` → Err (too short)
/// - `validate_postal_code_us("94110-")` → Err (incomplete +4)
///
/// # Errors
/// `ValidationError { code: "invalid_postal_code", ... }`.
pub fn validate_postal_code_us(s: &str) -> Result<(), ValidationError> {
    let bad = || {
        ValidationError::new(
            "invalid_postal_code",
            "Enter a valid US ZIP code (12345 or 12345-6789).",
        )
    };
    match s.split_once('-') {
        Some((first, second)) => {
            if first.len() != 5 || second.len() != 4 {
                return Err(bad());
            }
            if !first.chars().all(|c| c.is_ascii_digit())
                || !second.chars().all(|c| c.is_ascii_digit())
            {
                return Err(bad());
            }
            Ok(())
        }
        None => {
            if s.len() != 5 || !s.chars().all(|c| c.is_ascii_digit()) {
                return Err(bad());
            }
            Ok(())
        }
    }
}

/// Validate a Canadian postal code: 6 characters in the
/// alternating letter-digit-letter-space-digit-letter-digit
/// pattern (`A1A 1A1`). Letters must be uppercase ASCII.
///
/// Per Canada Post, the letters D, F, I, O, Q, U are NOT used in
/// the first position; and W, Z are not used as the first letter.
/// This validator does the format check only — it doesn't reject
/// codes with those letters (they'd just never be issued; making
/// a typo here is worth catching, but a strict version is a
/// follow-up).
///
/// # Errors
/// `ValidationError { code: "invalid_postal_code", ... }`.
pub fn validate_postal_code_ca(s: &str) -> Result<(), ValidationError> {
    let bad = || {
        ValidationError::new(
            "invalid_postal_code",
            "Enter a valid Canadian postal code (A1A 1A1).",
        )
    };
    if s.len() != 7 {
        return Err(bad());
    }
    let bytes = s.as_bytes();
    let is_uppercase_letter = |b: u8| b.is_ascii_uppercase();
    let is_digit = |b: u8| b.is_ascii_digit();
    if !is_uppercase_letter(bytes[0])
        || !is_digit(bytes[1])
        || !is_uppercase_letter(bytes[2])
        || bytes[3] != b' '
        || !is_digit(bytes[4])
        || !is_uppercase_letter(bytes[5])
        || !is_digit(bytes[6])
    {
        return Err(bad());
    }
    Ok(())
}

/// Validate a UK postcode in the canonical `OUTWARD INWARD` shape:
/// 1-2 outward characters + space + 3 inward characters.
///
/// The detailed UK postcode rules (Royal Mail BS7666) have several
/// allowed patterns; this validator covers the common shapes:
/// - `A9 9AA` (e.g. `M1 1AA`)
/// - `A99 9AA` (`B33 8TH`)
/// - `AA9 9AA` (`CR2 6XH`)
/// - `AA99 9AA` (`DN55 1PT`)
/// - `A9A 9AA` (`W1A 1AA`)
/// - `AA9A 9AA` (`EC1A 1BB`)
///
/// The inward part is always digit-letter-letter; outward is 2-4
/// characters mixing letters and digits per the patterns above.
/// Single mandatory space; letters must be uppercase.
///
/// # Errors
/// `ValidationError { code: "invalid_postal_code", ... }`.
pub fn validate_postal_code_uk(s: &str) -> Result<(), ValidationError> {
    let bad = || {
        ValidationError::new(
            "invalid_postal_code",
            "Enter a valid UK postcode (e.g. SW1A 1AA).",
        )
    };
    let (outward, inward) = s.split_once(' ').ok_or_else(bad)?;
    // Inward must be exactly 3: digit, letter, letter.
    if inward.len() != 3 {
        return Err(bad());
    }
    let inward_bytes = inward.as_bytes();
    if !inward_bytes[0].is_ascii_digit()
        || !inward_bytes[1].is_ascii_uppercase()
        || !inward_bytes[2].is_ascii_uppercase()
    {
        return Err(bad());
    }
    // Outward: 2-4 chars. First is letter. Last is digit OR letter
    // (`W1A`, `EC1A`). Middle chars follow specific patterns; for
    // a loose check we just require: first is letter, all chars
    // are uppercase-letter-or-digit, last position covers the
    // valid `Letter/Digit` set.
    if !(2..=4).contains(&outward.len()) {
        return Err(bad());
    }
    let outward_bytes = outward.as_bytes();
    if !outward_bytes[0].is_ascii_uppercase() {
        return Err(bad());
    }
    if !outward
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Err(bad());
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

/// Validate that `s` parses as either an IPv4 or IPv6 address.
/// Mirrors Django's `GenericIPAddressField(protocol="both")` (the
/// default). Issue #337 / Django-parity.
///
/// # Errors
/// `ValidationError { code: "invalid_ip_address", ... }` when the
/// value doesn't parse as either family.
pub fn validate_ip_address(s: &str) -> Result<(), ValidationError> {
    use std::str::FromStr as _;
    if std::net::Ipv4Addr::from_str(s).is_ok() || std::net::Ipv6Addr::from_str(s).is_ok() {
        return Ok(());
    }
    Err(ValidationError::new(
        "invalid_ip_address",
        "Enter a valid IPv4 or IPv6 address.",
    ))
}

/// Validate that `s` looks like a safe filesystem path string. Mirrors
/// the *structural* half of Django's `FilePathField` validation:
/// non-empty, no NUL bytes, no `..` parent-directory segments (path
/// traversal). Does NOT touch the filesystem — caller's responsibility
/// to verify existence + readability + sandbox membership when that
/// matters. Issue #338.
///
/// Accepted shapes:
/// - Relative: `docs/intro.md`, `assets/logo.png`
/// - Absolute (any platform): `/var/uploads/x.txt`, `C:\Users\me\f.txt`
/// - Trailing slash on directories
///
/// Rejected shapes:
/// - Empty string
/// - Strings containing `\0` (also caught by `validate_prohibit_null_characters`)
/// - Strings with a `..` segment between separators
///   (`docs/../etc/passwd`, `../secret`, `a/../b`)
///
/// # Errors
/// `ValidationError { code: "invalid_filepath", ... }` on any of the
/// rejected shapes above.
pub fn validate_filepath(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "invalid_filepath",
            "Enter a non-empty file path.",
        ));
    }
    if s.contains('\0') {
        return Err(ValidationError::new(
            "invalid_filepath",
            "File path must not contain NUL characters.",
        ));
    }
    // Reject `..` segments between separators. Split on both `/` and
    // `\` so Windows-style paths get the same defense.
    for segment in s.split(['/', '\\']) {
        if segment == ".." {
            return Err(ValidationError::new(
                "invalid_filepath",
                "File path must not contain `..` parent-directory segments.",
            ));
        }
    }
    Ok(())
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

    // -------- validate_creditcard_luhn --------

    #[test]
    fn luhn_accepts_known_valid_pans() {
        // Standard test PANs from the major card networks.
        // Visa
        assert!(validate_creditcard_luhn("4111111111111111").is_ok());
        // Mastercard
        assert!(validate_creditcard_luhn("5555555555554444").is_ok());
        // Amex (15 digits — within 12-19 range)
        assert!(validate_creditcard_luhn("378282246310005").is_ok());
        // Discover
        assert!(validate_creditcard_luhn("6011111111111117").is_ok());
    }

    #[test]
    fn luhn_strips_spaces_and_hyphens() {
        // Typical human-typed shapes.
        assert!(validate_creditcard_luhn("4111 1111 1111 1111").is_ok());
        assert!(validate_creditcard_luhn("4111-1111-1111-1111").is_ok());
        assert!(validate_creditcard_luhn(" 4111-1111 1111-1111 ").is_ok());
    }

    #[test]
    fn luhn_rejects_wrong_checksum() {
        // Off-by-one on the last digit: Luhn catches it.
        let e = validate_creditcard_luhn("4111111111111112").unwrap_err();
        assert_eq!(e.code, "invalid_card_number");
    }

    #[test]
    fn luhn_rejects_non_digit_chars() {
        assert!(validate_creditcard_luhn("4111-1111-1111-abcd").is_err());
    }

    #[test]
    fn luhn_rejects_too_short_or_too_long() {
        // 11 digits: too short. 20 digits: too long.
        assert!(validate_creditcard_luhn("41111111111").is_err());
        assert!(validate_creditcard_luhn("41111111111111111111").is_err());
    }

    #[test]
    fn luhn_rejects_empty_and_whitespace_only() {
        assert!(validate_creditcard_luhn("").is_err());
        assert!(validate_creditcard_luhn("   ").is_err());
    }

    // -------- validate_isbn --------

    #[test]
    fn isbn10_accepts_real_books() {
        // "The C Programming Language" — ISBN-10 0131103628.
        assert!(validate_isbn("0131103628").is_ok());
        // ISBN-10 ending in X (check digit = 10).
        assert!(validate_isbn("080442957X").is_ok());
        // Lower-case x also accepted.
        assert!(validate_isbn("080442957x").is_ok());
    }

    #[test]
    fn isbn13_accepts_real_books() {
        // "The C Programming Language" 2nd ed — ISBN-13 9780131103627.
        assert!(validate_isbn("9780131103627").is_ok());
    }

    #[test]
    fn isbn_strips_spaces_and_hyphens() {
        assert!(validate_isbn("0-13-110362-8").is_ok());
        assert!(validate_isbn("978-0-13-110362-7").is_ok());
        assert!(validate_isbn(" 978 0 13 110362 7 ").is_ok());
    }

    #[test]
    fn isbn_rejects_wrong_checksum() {
        // Flip the last digit of a known good ISBN.
        let e = validate_isbn("0131103627").unwrap_err();
        assert_eq!(e.code, "invalid_isbn");
        assert!(validate_isbn("9780131103620").is_err());
    }

    #[test]
    fn isbn_rejects_wrong_length() {
        // 11 / 14 digits — neither valid ISBN length.
        assert!(validate_isbn("01311036280").is_err());
        assert!(validate_isbn("97801311036270").is_err());
        assert!(validate_isbn("").is_err());
    }

    #[test]
    fn isbn_rejects_non_digit_chars() {
        // Letters in the middle are never valid.
        assert!(validate_isbn("01311a3628").is_err());
        // X only valid as the 10th digit of ISBN-10.
        assert!(validate_isbn("9780131103X27").is_err());
        assert!(validate_isbn("X131103628").is_err());
    }

    // -------- validate_hostname --------

    #[test]
    fn hostname_accepts_common_shapes() {
        assert!(validate_hostname("example.com").is_ok());
        assert!(validate_hostname("sub.example.co.uk").is_ok());
        assert!(validate_hostname("localhost").is_ok()); // single label
        assert!(validate_hostname("api-v1.example.com").is_ok()); // hyphen in middle
        assert!(validate_hostname("123.example.com").is_ok()); // numeric leading label
    }

    #[test]
    fn hostname_rejects_leading_or_trailing_hyphen() {
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("example-.com").is_err());
        assert!(validate_hostname("sub.-bad.com").is_err());
    }

    #[test]
    fn hostname_rejects_leading_or_trailing_dot() {
        assert!(validate_hostname(".example.com").is_err());
        assert!(validate_hostname("example.com.").is_err());
    }

    #[test]
    fn hostname_rejects_empty_label_between_dots() {
        assert!(validate_hostname("example..com").is_err());
    }

    #[test]
    fn hostname_rejects_invalid_chars() {
        assert!(validate_hostname("example.com/path").is_err());
        assert!(validate_hostname("ex_ample.com").is_err()); // underscore not allowed
        assert!(validate_hostname("ex ample.com").is_err());
        assert!(validate_hostname("café.com").is_err()); // ASCII only
    }

    #[test]
    fn hostname_rejects_oversize_label() {
        // 64-char label — 1 too long.
        let long_label: String = "a".repeat(64);
        assert!(validate_hostname(&format!("{long_label}.com")).is_err());
        // 63 chars is the max allowed.
        let max_label: String = "a".repeat(63);
        assert!(validate_hostname(&format!("{max_label}.com")).is_ok());
    }

    #[test]
    fn hostname_rejects_oversize_total() {
        // 254 chars total — 1 over the 253 cap.
        let label = "a".repeat(63);
        let too_long = format!("{label}.{label}.{label}.{label}xx"); // 63*4 + 3 dots + 2 = 257
        assert!(validate_hostname(&too_long).is_err());
    }

    #[test]
    fn hostname_rejects_empty() {
        assert!(validate_hostname("").is_err());
    }

    // -------- validate_iban --------

    #[test]
    fn iban_accepts_known_valid_examples() {
        // Standard test IBANs from various countries (ISO 13616).
        // UK
        assert!(validate_iban("GB82WEST12345698765432").is_ok());
        // Germany
        assert!(validate_iban("DE89370400440532013000").is_ok());
        // France
        assert!(validate_iban("FR1420041010050500013M02606").is_ok());
        // Norway (shortest standard IBAN — 15 chars)
        assert!(validate_iban("NO9386011117947").is_ok());
    }

    #[test]
    fn iban_strips_spaces() {
        // Printed form is space-grouped.
        assert!(validate_iban("GB82 WEST 1234 5698 7654 32").is_ok());
    }

    #[test]
    fn iban_rejects_wrong_checksum() {
        // Flip a digit in the check region.
        let e = validate_iban("GB82WEST12345698765431").unwrap_err();
        assert_eq!(e.code, "invalid_iban");
    }

    #[test]
    fn iban_rejects_wrong_format() {
        // Lowercase country code rejected (must be uppercase).
        assert!(validate_iban("gb82WEST12345698765432").is_err());
        // First 2 must be letters, next 2 digits.
        assert!(validate_iban("1B82WEST12345698765432").is_err());
        assert!(validate_iban("GBAB12345678901234567890").is_err());
        // Non-alphanumeric chars after the prefix rejected.
        assert!(validate_iban("GB82WEST!2345698765432").is_err());
    }

    #[test]
    fn iban_rejects_out_of_range_length() {
        // 4 chars total — below the 5-char floor.
        assert!(validate_iban("GB82").is_err());
        // 35 chars total — above the 34-char ceiling.
        let too_long = format!("GB82{}", "X".repeat(31));
        assert!(validate_iban(&too_long).is_err());
    }

    #[test]
    fn iban_rejects_empty_and_whitespace_only() {
        assert!(validate_iban("").is_err());
        assert!(validate_iban("   ").is_err());
    }

    // -------- validate_mac_address --------

    #[test]
    fn mac_accepts_colon_separated() {
        assert!(validate_mac_address("00:1A:2B:3C:4D:5E").is_ok());
        assert!(validate_mac_address("FF:FF:FF:FF:FF:FF").is_ok());
        assert!(validate_mac_address("00:00:00:00:00:00").is_ok());
    }

    #[test]
    fn mac_accepts_hyphen_separated() {
        assert!(validate_mac_address("00-1A-2B-3C-4D-5E").is_ok());
    }

    #[test]
    fn mac_accepts_lowercase_hex() {
        assert!(validate_mac_address("00:1a:2b:3c:4d:5e").is_ok());
        assert!(validate_mac_address("ff:ff:ff:ff:ff:ff").is_ok());
    }

    #[test]
    fn mac_rejects_no_separators() {
        // Bare 12-hex form (Cisco style) intentionally NOT supported
        // here — operators typing into a form expect : or - to
        // separate octets, and accepting both with-and-without
        // makes UX murky.
        assert!(validate_mac_address("001A2B3C4D5E").is_err());
    }

    #[test]
    fn mac_rejects_mixed_separators() {
        assert!(validate_mac_address("00:1A:2B-3C:4D:5E").is_err());
        assert!(validate_mac_address("00-1A:2B:3C:4D:5E").is_err());
    }

    #[test]
    fn mac_rejects_non_hex_chars() {
        assert!(validate_mac_address("00:1A:2B:3C:4D:5Z").is_err());
        assert!(validate_mac_address("00:1G:2B:3C:4D:5E").is_err());
    }

    #[test]
    fn mac_rejects_wrong_octet_length() {
        // Single-digit octets rejected (operators should zero-pad).
        assert!(validate_mac_address("0:1A:2B:3C:4D:5E").is_err());
        // Triple-digit octets rejected.
        assert!(validate_mac_address("000:1A:2B:3C:4D:5E").is_err());
    }

    #[test]
    fn mac_rejects_wrong_total_length() {
        assert!(validate_mac_address("").is_err());
        assert!(validate_mac_address("00:1A:2B:3C:4D").is_err()); // 5 octets
        assert!(validate_mac_address("00:1A:2B:3C:4D:5E:6F").is_err()); // 7 octets
    }

    // -------- validate_base64 --------

    #[test]
    fn base64_accepts_standard_alphabet() {
        // "Hello" → "SGVsbG8="
        assert!(validate_base64("SGVsbG8=").is_ok());
        // "Many hands" → "TWFueSBoYW5kcw=="
        assert!(validate_base64("TWFueSBoYW5kcw==").is_ok());
        // No padding needed when len % 3 == 0.
        assert!(validate_base64("abcd").is_ok());
    }

    #[test]
    fn base64_accepts_plus_and_slash() {
        // "?>>>" → "Pz4+Pg==" (uses + character).
        // Build a likely-valid string with + and /.
        assert!(validate_base64("AB+/").is_ok());
    }

    #[test]
    fn base64_rejects_urlsafe_chars() {
        // - and _ are URL-safe, not standard.
        assert!(validate_base64("AB-_").is_err());
    }

    #[test]
    fn base64_rejects_bad_padding_count() {
        // Three trailing = is never valid.
        assert!(validate_base64("AB===").is_err());
    }

    #[test]
    fn base64_rejects_non_multiple_of_4() {
        // Standard base64 with any padding must be a multiple of 4.
        assert!(validate_base64("ABCDE=").is_err()); // 6 chars, not 4n
    }

    #[test]
    fn base64_rejects_empty() {
        assert!(validate_base64("").is_err());
        assert!(validate_base64_urlsafe("").is_err());
    }

    // -------- validate_base64_urlsafe --------

    #[test]
    fn base64_urlsafe_accepts_dash_and_underscore() {
        assert!(validate_base64_urlsafe("AB-_").is_ok());
        assert!(validate_base64_urlsafe("SGVsbG8=").is_ok()); // standard chars also fine
    }

    #[test]
    fn base64_urlsafe_rejects_plus_and_slash() {
        assert!(validate_base64_urlsafe("AB+/").is_err());
    }

    #[test]
    fn base64_urlsafe_accepts_unpadded() {
        // URL-safe base64 commonly omits padding (JWT, etc.).
        // 5 chars, no padding — allowed for url-safe.
        assert!(validate_base64_urlsafe("ABCDE").is_ok());
    }

    // -------- validate_jwt_shape --------

    #[test]
    fn jwt_shape_accepts_valid_three_segments() {
        // Canonical JWT example: header.payload.signature
        // (3 URL-safe base64 segments, each non-empty).
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                   eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0.\
                   SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        assert!(validate_jwt_shape(jwt).is_ok());
    }

    #[test]
    fn jwt_shape_rejects_wrong_segment_count() {
        assert!(validate_jwt_shape("abc.def").is_err()); // 2 segments
        assert!(validate_jwt_shape("a.b.c.d").is_err()); // 4 segments
        assert!(validate_jwt_shape("no-dots").is_err());
    }

    #[test]
    fn jwt_shape_rejects_empty_segments() {
        assert!(validate_jwt_shape(".payload.sig").is_err());
        assert!(validate_jwt_shape("header..sig").is_err());
        assert!(validate_jwt_shape("header.payload.").is_err());
    }

    #[test]
    fn jwt_shape_rejects_non_urlsafe_chars_in_segment() {
        // `+` and `/` are standard-base64, not URL-safe — JWT spec
        // uses URL-safe alphabet.
        assert!(validate_jwt_shape("abc.de+f.ghi").is_err());
        assert!(validate_jwt_shape("abc.def.gh/i").is_err());
    }

    #[test]
    fn jwt_shape_rejects_empty() {
        assert!(validate_jwt_shape("").is_err());
    }

    // -------- validate_semver --------

    #[test]
    fn semver_accepts_canonical_form() {
        assert!(validate_semver("1.0.0").is_ok());
        assert!(validate_semver("0.0.1").is_ok());
        assert!(validate_semver("10.20.30").is_ok());
    }

    #[test]
    fn semver_accepts_pre_release() {
        assert!(validate_semver("1.0.0-alpha").is_ok());
        assert!(validate_semver("1.0.0-alpha.1").is_ok());
        assert!(validate_semver("1.0.0-0.3.7").is_ok());
        assert!(validate_semver("1.0.0-x-y-z.--").is_ok());
    }

    #[test]
    fn semver_accepts_build_metadata() {
        assert!(validate_semver("1.0.0+20130313144700").is_ok());
        assert!(validate_semver("1.0.0+exp.sha.5114f85").is_ok());
        // Build metadata IS allowed to have leading-zero numeric ids.
        assert!(validate_semver("1.0.0+007").is_ok());
    }

    #[test]
    fn semver_accepts_full_form() {
        assert!(validate_semver("1.0.0-rc.1+build.42").is_ok());
    }

    #[test]
    fn semver_rejects_missing_core_parts() {
        assert!(validate_semver("1").is_err());
        assert!(validate_semver("1.0").is_err());
        assert!(validate_semver("1.0.0.0").is_err());
        assert!(validate_semver("").is_err());
    }

    #[test]
    fn semver_rejects_leading_zero_in_core() {
        assert!(validate_semver("01.0.0").is_err());
        assert!(validate_semver("1.02.0").is_err());
        assert!(validate_semver("1.0.03").is_err());
        // But "0" itself is fine.
        assert!(validate_semver("0.0.0").is_ok());
    }

    #[test]
    fn semver_rejects_leading_zero_in_numeric_prerelease_id() {
        // Numeric pre-release IDs may NOT have leading zeros.
        assert!(validate_semver("1.0.0-01").is_err());
        assert!(validate_semver("1.0.0-alpha.01").is_err());
    }

    #[test]
    fn semver_rejects_empty_prerelease_or_build() {
        assert!(validate_semver("1.0.0-").is_err());
        assert!(validate_semver("1.0.0+").is_err());
        assert!(validate_semver("1.0.0-alpha..1").is_err());
    }

    #[test]
    fn semver_rejects_invalid_chars() {
        assert!(validate_semver("1.0.0-alpha_1").is_err()); // underscore not allowed
        assert!(validate_semver("1.0.0-alpha 1").is_err()); // space
        assert!(validate_semver("v1.0.0").is_err()); // leading v
    }

    // -------- validate_country_code --------

    #[test]
    fn country_code_accepts_two_uppercase_letters() {
        assert!(validate_country_code("US").is_ok());
        assert!(validate_country_code("GB").is_ok());
        assert!(validate_country_code("DE").is_ok());
        assert!(validate_country_code("JP").is_ok());
        // The format check accepts ANY 2 uppercase letters — `ZZ`
        // isn't a real country, but full validation against ISO
        // 3166-1 needs a maintained list (documented).
        assert!(validate_country_code("ZZ").is_ok());
    }

    #[test]
    fn country_code_rejects_wrong_length() {
        assert!(validate_country_code("U").is_err());
        assert!(validate_country_code("USA").is_err()); // alpha-3, not alpha-2
        assert!(validate_country_code("").is_err());
    }

    #[test]
    fn country_code_rejects_lowercase_or_non_letters() {
        assert!(validate_country_code("us").is_err());
        assert!(validate_country_code("Us").is_err());
        assert!(validate_country_code("U1").is_err());
        assert!(validate_country_code("U-").is_err());
    }

    // -------- validate_currency_code --------

    #[test]
    fn currency_code_accepts_three_uppercase_letters() {
        assert!(validate_currency_code("USD").is_ok());
        assert!(validate_currency_code("EUR").is_ok());
        assert!(validate_currency_code("GBP").is_ok());
        assert!(validate_currency_code("JPY").is_ok());
        assert!(validate_currency_code("XXX").is_ok()); // ZZZ-style
    }

    #[test]
    fn currency_code_rejects_wrong_length() {
        assert!(validate_currency_code("US").is_err());
        assert!(validate_currency_code("USDD").is_err());
        assert!(validate_currency_code("").is_err());
    }

    #[test]
    fn currency_code_rejects_lowercase_or_non_letters() {
        assert!(validate_currency_code("usd").is_err());
        assert!(validate_currency_code("UsD").is_err());
        assert!(validate_currency_code("U5D").is_err());
    }

    // -------- validate_language_tag --------

    #[test]
    fn language_tag_accepts_bare_lang() {
        assert!(validate_language_tag("en").is_ok());
        assert!(validate_language_tag("fr").is_ok());
        assert!(validate_language_tag("zh").is_ok());
        // 3-letter language (ISO 639-2/3).
        assert!(validate_language_tag("eng").is_ok());
    }

    #[test]
    fn language_tag_accepts_lang_with_region() {
        assert!(validate_language_tag("en-US").is_ok());
        assert!(validate_language_tag("fr-CA").is_ok());
        assert!(validate_language_tag("pt-BR").is_ok());
        // Numeric UN region code (es-419 = Latin American Spanish).
        assert!(validate_language_tag("es-419").is_ok());
    }

    #[test]
    fn language_tag_accepts_lang_with_script() {
        assert!(validate_language_tag("zh-Hans").is_ok());
        assert!(validate_language_tag("sr-Cyrl").is_ok());
        // Script + region.
        assert!(validate_language_tag("zh-Hans-CN").is_ok());
    }

    #[test]
    fn language_tag_rejects_uppercase_lang() {
        assert!(validate_language_tag("EN").is_err());
        assert!(validate_language_tag("En").is_err());
    }

    #[test]
    fn language_tag_rejects_wrong_region_case() {
        assert!(validate_language_tag("en-us").is_err());
        assert!(validate_language_tag("en-Us").is_err());
    }

    #[test]
    fn language_tag_rejects_wrong_lang_length() {
        assert!(validate_language_tag("e").is_err());
        assert!(validate_language_tag("english").is_err());
    }

    #[test]
    fn language_tag_rejects_too_many_parts() {
        // Variants / extensions / private-use not supported here.
        assert!(validate_language_tag("en-US-x-something").is_err());
    }

    #[test]
    fn language_tag_rejects_empty_or_garbage() {
        assert!(validate_language_tag("").is_err());
        assert!(validate_language_tag("not a tag").is_err());
        assert!(validate_language_tag("en_US").is_err()); // wrong separator
    }

    // -------- validate_postal_code_us --------

    #[test]
    fn postal_code_us_accepts_five_digit_zip() {
        assert!(validate_postal_code_us("94110").is_ok());
        assert!(validate_postal_code_us("00501").is_ok()); // valid northeast ZIP
        assert!(validate_postal_code_us("99950").is_ok());
    }

    #[test]
    fn postal_code_us_accepts_zip_plus_four() {
        assert!(validate_postal_code_us("12345-6789").is_ok());
        assert!(validate_postal_code_us("94110-0001").is_ok());
    }

    #[test]
    fn postal_code_us_rejects_wrong_length() {
        assert!(validate_postal_code_us("1234").is_err()); // 4 digits
        assert!(validate_postal_code_us("123456").is_err()); // 6 digits
        assert!(validate_postal_code_us("123456789").is_err()); // 9 digits without hyphen
    }

    #[test]
    fn postal_code_us_rejects_bad_plus_four_shape() {
        assert!(validate_postal_code_us("94110-").is_err()); // missing +4
        assert!(validate_postal_code_us("9411-12345").is_err()); // wrong prefix length
        assert!(validate_postal_code_us("94110-12").is_err()); // wrong suffix length
        assert!(validate_postal_code_us("94110-123A").is_err()); // non-digit in +4
    }

    #[test]
    fn postal_code_us_rejects_non_digit_in_base() {
        assert!(validate_postal_code_us("9411A").is_err());
        assert!(validate_postal_code_us("").is_err());
    }

    // -------- validate_postal_code_ca --------

    #[test]
    fn postal_code_ca_accepts_canonical_shape() {
        assert!(validate_postal_code_ca("K1A 0B1").is_ok()); // Parliament Hill
        assert!(validate_postal_code_ca("M5W 1E6").is_ok()); // Toronto
        assert!(validate_postal_code_ca("V6B 4Y8").is_ok()); // Vancouver
    }

    #[test]
    fn postal_code_ca_rejects_lowercase_letters() {
        assert!(validate_postal_code_ca("k1a 0b1").is_err());
        assert!(validate_postal_code_ca("K1a 0B1").is_err());
    }

    #[test]
    fn postal_code_ca_rejects_missing_space_or_wrong_separator() {
        assert!(validate_postal_code_ca("K1A0B1").is_err()); // no space
        assert!(validate_postal_code_ca("K1A-0B1").is_err()); // hyphen
        assert!(validate_postal_code_ca("K1A  0B1").is_err()); // double space
    }

    #[test]
    fn postal_code_ca_rejects_wrong_pattern() {
        assert!(validate_postal_code_ca("1AB 0B1").is_err()); // starts with digit
        assert!(validate_postal_code_ca("AAA 0B1").is_err()); // all letters
        assert!(validate_postal_code_ca("K1A 000").is_err()); // all digits
        assert!(validate_postal_code_ca("").is_err());
    }

    // -------- validate_postal_code_uk --------

    #[test]
    fn postal_code_uk_accepts_canonical_shapes() {
        // All the patterns from the docstring.
        assert!(validate_postal_code_uk("M1 1AA").is_ok()); // A9 9AA
        assert!(validate_postal_code_uk("B33 8TH").is_ok()); // A99 9AA
        assert!(validate_postal_code_uk("CR2 6XH").is_ok()); // AA9 9AA
        assert!(validate_postal_code_uk("DN55 1PT").is_ok()); // AA99 9AA
        assert!(validate_postal_code_uk("W1A 1AA").is_ok()); // A9A 9AA
        assert!(validate_postal_code_uk("EC1A 1BB").is_ok()); // AA9A 9AA
        assert!(validate_postal_code_uk("SW1A 1AA").is_ok()); // Downing Street
    }

    #[test]
    fn postal_code_uk_rejects_missing_space() {
        assert!(validate_postal_code_uk("SW1A1AA").is_err());
    }

    #[test]
    fn postal_code_uk_rejects_lowercase() {
        assert!(validate_postal_code_uk("sw1a 1aa").is_err());
    }

    #[test]
    fn postal_code_uk_rejects_wrong_inward_length() {
        assert!(validate_postal_code_uk("M1 1A").is_err());
        assert!(validate_postal_code_uk("M1 1AAA").is_err());
    }

    #[test]
    fn postal_code_uk_rejects_wrong_inward_pattern() {
        // Inward must be digit-letter-letter.
        assert!(validate_postal_code_uk("M1 AAA").is_err()); // letter-letter-letter
        assert!(validate_postal_code_uk("M1 123").is_err()); // all digits
    }

    #[test]
    fn postal_code_uk_rejects_outward_starting_with_digit() {
        assert!(validate_postal_code_uk("1A 1AA").is_err());
    }

    #[test]
    fn postal_code_uk_rejects_empty() {
        assert!(validate_postal_code_uk("").is_err());
    }
}
