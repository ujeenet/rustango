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
//! - `validate_min_length` / `validate_max_length` — string char count.
//! - `validate_min_value` / `validate_max_value` — i64 numeric bounds.
//! - `validate_integer` — parses as `i64`.
//! - `validate_decimal` — `max_digits` + `decimal_places` bounds
//!   (Django's `DecimalValidator`).
//! - `validate_ipv4_address` / `validate_ipv6_address` — dotted-quad /
//!   colon-hex address shape via `std::net::Ipv4Addr` / `Ipv6Addr`.
//! - `validate_comma_separated_integer_list` — `"1,2,3"`. Django's
//!   `validate_comma_separated_integer_list`.
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
}
