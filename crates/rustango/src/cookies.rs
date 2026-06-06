//! Django-shape `Set-Cookie` builder.
//!
//! Mirrors `django.http.HttpResponse.set_cookie(key, value, max_age=None,
//! expires=None, path='/', domain=None, secure=False, httponly=False,
//! samesite=None)`. Produces the `Set-Cookie` header value as a string
//! (and an axum [`HeaderValue`] convenience).
//!
//! ```ignore
//! use rustango::cookies::{Cookie, SameSite};
//! use std::time::Duration;
//!
//! // Build → render.
//! let header = Cookie::new("session", "abc123")
//!     .path("/")
//!     .http_only()
//!     .secure()
//!     .same_site(SameSite::Lax)
//!     .max_age(Duration::from_secs(3600))
//!     .build();
//! // -> "session=abc123; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=3600"
//!
//! // Delete a cookie (Django's `response.delete_cookie(key)`).
//! let header = Cookie::deletion("session", "/").build();
//! // -> "session=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
//! ```
//!
//! ## Why not the `cookie` crate?
//!
//! The `cookie` crate (used by axum-extra / tower-cookies) is the
//! production-grade choice for parsing + signed/private cookies.
//! This module is intentionally smaller — just the `Set-Cookie`
//! emission path Django code translates to most often. The output
//! is a plain `String` (or `HeaderValue`), so it composes cleanly
//! with whatever cookie crate the project uses for parsing.
//!
//! Doesn't validate cookie names against RFC 6265 token rules —
//! caller is responsible for sane names (the `messages` module
//! used `rustango_messages`, the `csrf` module uses `csrftoken`,
//! etc.). Empty names build cleanly but produce a malformed header
//! that browsers will reject; that's a caller bug, not a crate one.

use std::time::Duration;

/// Django-parity `SameSite` cookie attribute values. Default `None`
/// in the cookie struct means the attribute is omitted entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// `SameSite=Strict` — cookie withheld on all cross-site requests.
    Strict,
    /// `SameSite=Lax` — default browser behavior; allowed on top-
    /// level cross-site GET navigations. **Recommended for session
    /// cookies.**
    Lax,
    /// `SameSite=None` — cookie sent on all cross-site requests.
    /// Browsers require `Secure` alongside `None` since Chrome 80.
    None,
}

impl SameSite {
    /// String form for the header value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

/// Django-shape cookie builder. Construct via [`Cookie::new`] or
/// [`Cookie::deletion`]; chain attribute methods; finalize with
/// [`Cookie::build`] (string) or [`Cookie::header_value`]
/// (`axum::http::HeaderValue` — falls back to a panic-safe form on
/// invalid bytes).
#[derive(Debug, Clone)]
pub struct Cookie {
    name: String,
    value: String,
    path: Option<String>,
    domain: Option<String>,
    max_age: Option<i64>,
    expires: Option<String>,
    http_only: bool,
    secure: bool,
    same_site: Option<SameSite>,
}

impl Cookie {
    /// Start a new cookie with the given name + value. Default
    /// attributes: no `Path`, no `Domain`, no `Max-Age`, no
    /// `Expires`, NO `HttpOnly` / `Secure` / `SameSite`. Chain
    /// builder methods to set them.
    ///
    /// Django defaults `Path=/`; we don't apply that here so the
    /// builder is composable — callers wire `.path("/")` explicitly
    /// (most modern axum cookies do).
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            path: None,
            domain: None,
            max_age: None,
            expires: None,
            http_only: false,
            secure: false,
            same_site: None,
        }
    }

    /// Django-parity `response.delete_cookie(key, path='/', domain=None)`.
    /// Returns a builder pre-set to expire the cookie at the Unix
    /// epoch (`Max-Age=0` + `Expires=Thu, 01 Jan 1970 00:00:00 GMT`)
    /// so browsers drop it on receipt.
    ///
    /// `path` must match the original `Set-Cookie`'s path — browsers
    /// scope the delete to the path. Django defaults to `/`; pass
    /// the same path you used on creation.
    #[must_use]
    pub fn deletion(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, "")
            .path(path)
            .max_age(Duration::from_secs(0))
            .expires_at_epoch()
    }

    /// `Path=<path>` attribute. Scopes which request paths the
    /// browser sends the cookie on. Most projects want `"/"`.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// `Domain=<domain>` attribute. Without it the cookie is
    /// host-only (sent only to the exact host that set it).
    #[must_use]
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// `Max-Age=<secs>` attribute. Browsers prefer this over
    /// `Expires` when both are set.
    #[must_use]
    pub fn max_age(mut self, ttl: Duration) -> Self {
        self.max_age = Some(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX));
        self
    }

    /// Explicit `Expires=<http-date>` attribute. Most callers prefer
    /// [`Self::max_age`] which is relative + clock-independent.
    /// Pass an RFC 1123 IMF-fixdate string
    /// (`"Thu, 01 Jan 1970 00:00:00 GMT"`); pair with
    /// [`crate::http_date::http_date`] to format from a Unix
    /// timestamp.
    #[must_use]
    pub fn expires(mut self, http_date_str: impl Into<String>) -> Self {
        self.expires = Some(http_date_str.into());
        self
    }

    /// Pre-set `Expires` to the Unix epoch — convenience shorthand
    /// for [`Self::deletion`]. Useful when chaining manually.
    #[must_use]
    pub fn expires_at_epoch(mut self) -> Self {
        self.expires = Some("Thu, 01 Jan 1970 00:00:00 GMT".to_owned());
        self
    }

    /// Set the `HttpOnly` flag — cookie unreachable from JS
    /// (defends against XSS-stealing session cookies). Default
    /// `false`.
    #[must_use]
    pub fn http_only(mut self) -> Self {
        self.http_only = true;
        self
    }

    /// Set the `Secure` flag — cookie only sent over HTTPS.
    /// Required when `SameSite=None`.
    #[must_use]
    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    /// Set the `SameSite=<value>` attribute. See [`SameSite`] for
    /// strict / lax / none semantics.
    #[must_use]
    pub fn same_site(mut self, value: SameSite) -> Self {
        self.same_site = Some(value);
        self
    }

    /// Build the final `Set-Cookie` header value as a `String`.
    /// Suitable for `axum::http::HeaderValue::from_str`.
    ///
    /// Attribute order follows the Django shape:
    /// `<name>=<value>; Path; Domain; Max-Age; Expires; HttpOnly;
    /// Secure; SameSite`.
    #[must_use]
    pub fn build(&self) -> String {
        let mut s = String::with_capacity(64);
        s.push_str(&self.name);
        s.push('=');
        s.push_str(&self.value);
        if let Some(p) = &self.path {
            s.push_str("; Path=");
            s.push_str(p);
        }
        if let Some(d) = &self.domain {
            s.push_str("; Domain=");
            s.push_str(d);
        }
        if let Some(m) = self.max_age {
            use std::fmt::Write as _;
            let _ = write!(s, "; Max-Age={m}");
        }
        if let Some(e) = &self.expires {
            s.push_str("; Expires=");
            s.push_str(e);
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        if self.secure {
            s.push_str("; Secure");
        }
        if let Some(ss) = self.same_site {
            s.push_str("; SameSite=");
            s.push_str(ss.as_str());
        }
        s
    }

    /// Build as an `axum::http::HeaderValue` directly. Returns
    /// `None` if the rendered string contains bytes axum considers
    /// invalid (control characters, etc.) — should never happen
    /// with sane caller-supplied names/values, but exposed as
    /// `Option` to avoid panicking on attacker-controlled input.
    #[must_use]
    pub fn header_value(&self) -> Option<axum::http::HeaderValue> {
        axum::http::HeaderValue::from_str(&self.build()).ok()
    }
}

/// [`django.utils.http.parse_cookie`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.http.parse_cookie) —
/// parse a `Cookie:` header value into a name → value map.
///
/// Splits on `;`, then each chunk on the first `=`. Whitespace
/// around keys / values is trimmed. Quoted values
/// (`name="value with spaces"`) have their surrounding quotes
/// stripped per RFC 6265 §5.2. Malformed chunks (no `=`, empty
/// key) are skipped; well-formed chunks after a malformed one
/// still parse.
///
/// Use when you need to parse a raw `Cookie:` header outside an
/// axum request (test fixtures, manual proxying, header-replay
/// audits). For axum handler code prefer the `axum-extra`
/// `CookieJar` extractor, which uses the same parsing rules.
///
/// ```
/// use rustango::cookies::parse_cookie_header;
/// let cookies = parse_cookie_header("sessionid=abc123; csrftoken=xyz");
/// assert_eq!(cookies.get("sessionid"), Some(&"abc123".to_owned()));
/// assert_eq!(cookies.get("csrftoken"), Some(&"xyz".to_owned()));
///
/// // Quoted value — surrounding `"` stripped.
/// let cookies = parse_cookie_header(r#"pref="dark mode""#);
/// assert_eq!(cookies.get("pref"), Some(&"dark mode".to_owned()));
///
/// // Empty input → empty map.
/// assert!(parse_cookie_header("").is_empty());
/// ```
#[must_use]
pub fn parse_cookie_header(header: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for chunk in header.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let Some((key, val)) = chunk.split_once('=') else {
            // Malformed chunk (no `=`) — skip silently, matching
            // Django's "ignore weirdness, decode what we can" shape.
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let val = val.trim();
        // RFC 6265 §5.2 — strip surrounding double-quotes if both
        // ends are quoted. Single quote on one end stays verbatim.
        let unquoted = if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
            &val[1..val.len() - 1]
        } else {
            val
        };
        out.insert(key.to_owned(), unquoted.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- basic shape --------

    #[test]
    fn minimal_cookie_renders_just_name_equals_value() {
        let s = Cookie::new("session", "abc").build();
        assert_eq!(s, "session=abc");
    }

    #[test]
    fn empty_value_renders_cleanly() {
        // Common shape during a deletion before path/expires kick in.
        let s = Cookie::new("session", "").build();
        assert_eq!(s, "session=");
    }

    // -------- attribute coverage --------

    #[test]
    fn path_attribute() {
        assert_eq!(Cookie::new("k", "v").path("/").build(), "k=v; Path=/");
    }

    #[test]
    fn domain_attribute() {
        assert_eq!(
            Cookie::new("k", "v").domain(".example.com").build(),
            "k=v; Domain=.example.com"
        );
    }

    #[test]
    fn max_age_attribute() {
        assert_eq!(
            Cookie::new("k", "v")
                .max_age(Duration::from_secs(3600))
                .build(),
            "k=v; Max-Age=3600"
        );
    }

    #[test]
    fn expires_attribute() {
        assert_eq!(
            Cookie::new("k", "v")
                .expires("Thu, 01 Jan 1970 00:00:00 GMT")
                .build(),
            "k=v; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn http_only_flag() {
        assert_eq!(Cookie::new("k", "v").http_only().build(), "k=v; HttpOnly");
    }

    #[test]
    fn secure_flag() {
        assert_eq!(Cookie::new("k", "v").secure().build(), "k=v; Secure");
    }

    #[test]
    fn same_site_lax() {
        assert_eq!(
            Cookie::new("k", "v").same_site(SameSite::Lax).build(),
            "k=v; SameSite=Lax"
        );
    }

    #[test]
    fn same_site_strict() {
        assert_eq!(
            Cookie::new("k", "v").same_site(SameSite::Strict).build(),
            "k=v; SameSite=Strict"
        );
    }

    #[test]
    fn same_site_none() {
        assert_eq!(
            Cookie::new("k", "v").same_site(SameSite::None).build(),
            "k=v; SameSite=None"
        );
    }

    // -------- attribute ordering --------

    #[test]
    fn full_attribute_set_renders_in_django_order() {
        let s = Cookie::new("session", "abc")
            .path("/")
            .domain("example.com")
            .max_age(Duration::from_secs(3600))
            .expires("Sun, 06 Nov 1994 08:49:37 GMT")
            .http_only()
            .secure()
            .same_site(SameSite::Lax)
            .build();
        assert_eq!(
            s,
            "session=abc; Path=/; Domain=example.com; Max-Age=3600; Expires=Sun, 06 Nov 1994 08:49:37 GMT; HttpOnly; Secure; SameSite=Lax"
        );
    }

    // -------- deletion (Django parity) --------

    #[test]
    fn deletion_shape() {
        // Django `response.delete_cookie('session')` — the cookie
        // gets set to empty with Max-Age=0 + Expires=epoch so the
        // browser drops it.
        let s = Cookie::deletion("session", "/").build();
        assert_eq!(
            s,
            "session=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn deletion_with_domain_for_subdomain_scoping() {
        // Browsers scope deletes by (name, path, domain). Subdomain
        // cookies need the same Domain on delete.
        let s = Cookie::deletion("session", "/")
            .domain(".example.com")
            .build();
        assert!(s.contains("Domain=.example.com"));
        assert!(s.contains("Max-Age=0"));
    }

    // -------- header_value --------

    #[test]
    fn header_value_returns_some_for_normal_cookie() {
        let v = Cookie::new("session", "abc").path("/").header_value();
        assert!(v.is_some());
    }

    #[test]
    fn header_value_returns_none_for_invalid_chars() {
        // A NUL byte in the value should make axum reject the
        // HeaderValue construction.
        let v = Cookie::new("session", "a\0b").header_value();
        assert!(v.is_none(), "axum should reject NUL bytes in headers");
    }

    // -------- expires_at_epoch convenience --------

    #[test]
    fn expires_at_epoch_uses_canonical_imf_fixdate() {
        let s = Cookie::new("k", "").expires_at_epoch().build();
        assert!(
            s.contains("Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
            "got: {s}"
        );
    }

    // -------- parse_cookie_header --------

    #[test]
    fn parse_cookie_header_basic() {
        let m = parse_cookie_header("sessionid=abc123; csrftoken=xyz");
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("sessionid"), Some(&"abc123".to_owned()));
        assert_eq!(m.get("csrftoken"), Some(&"xyz".to_owned()));
    }

    #[test]
    fn parse_cookie_header_trims_whitespace() {
        let m = parse_cookie_header("  a = 1 ;  b=2  ");
        assert_eq!(m.get("a"), Some(&"1".to_owned()));
        assert_eq!(m.get("b"), Some(&"2".to_owned()));
    }

    #[test]
    fn parse_cookie_header_strips_quoted_value() {
        // Both-sides quoted → strip both quotes.
        let m = parse_cookie_header(r#"pref="dark mode""#);
        assert_eq!(m.get("pref"), Some(&"dark mode".to_owned()));
        // Single-sided quote stays verbatim.
        let m = parse_cookie_header(r#"x="not closed"#);
        assert_eq!(m.get("x"), Some(&"\"not closed".to_owned()));
    }

    #[test]
    fn parse_cookie_header_empty_input() {
        assert!(parse_cookie_header("").is_empty());
        assert!(parse_cookie_header("   ").is_empty());
        // All semicolons — no actual chunks.
        assert!(parse_cookie_header(";;;").is_empty());
    }

    #[test]
    fn parse_cookie_header_skips_malformed_chunks() {
        // "no-equals" chunk → skipped; "good=value" → kept.
        let m = parse_cookie_header("no-equals; good=value");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("good"), Some(&"value".to_owned()));
    }

    #[test]
    fn parse_cookie_header_skips_empty_keys() {
        // "=val" → empty key, skipped.
        let m = parse_cookie_header("=val; ok=1");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("ok"), Some(&"1".to_owned()));
    }

    #[test]
    fn parse_cookie_header_handles_value_with_equals() {
        // First `=` wins — value can contain `=`.
        let m = parse_cookie_header("token=abc=xyz==");
        assert_eq!(m.get("token"), Some(&"abc=xyz==".to_owned()));
    }

    #[test]
    fn parse_cookie_header_empty_value() {
        // `name=` with nothing after.
        let m = parse_cookie_header("name=");
        assert_eq!(m.get("name"), Some(&String::new()));
    }
}
