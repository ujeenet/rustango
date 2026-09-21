//! A `Set-Cookie` builder, plus a parser for a `Cookie:` header.
//!
//! [`Cookie::build`](crate::cookies::Cookie::build) gives the header
//! value as a `String`;
//! `header_value` gives an axum `HeaderValue`.
//!
//! No flag is on by default. A cookie that carries a session or any
//! other sensitive value needs `.http_only()`, `.secure()` and a
//! `SameSite` set on it by hand.
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
//! // Delete a cookie.
//! let header = Cookie::deletion("session", "/").build();
//! // -> "session=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
//! ```
//!
//! ## Why not the `cookie` crate?
//!
//! The `cookie` crate, which axum-extra and tower-cookies use, is
//! the fuller choice, with parsing and signed or private cookies.
//! This module covers only the `Set-Cookie` side, and its output is
//! a plain string, so it
//! composes with whatever crate you use elsewhere.
//!
//! Names are not checked against the RFC 6265 token rules. Pass a
//! sane name. An empty or odd name builds fine here but produces a
//! header the browser will reject.

use std::time::Duration;

/// Values for the `SameSite` attribute. Leaving it unset on a
/// [`Cookie`] leaves the attribute out of the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// Never sent on a cross-site request.
    Strict,
    /// Sent on a top-level cross-site GET only. **Use this for a
    /// session cookie.**
    Lax,
    /// Sent on every cross-site request. Browsers reject it unless
    /// `Secure` is set too, so always pair it with
    /// [`Cookie::secure`].
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

/// Cookie builder. Start with [`Cookie::new`] or
/// [`Cookie::deletion`], chain the attribute methods, and finish
/// with [`Cookie::build`].
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
    /// Start a cookie with this name and value. Nothing else is set:
    /// no `Path`, `Domain`, `Max-Age` or `Expires`, and **no
    /// `HttpOnly`, `Secure` or `SameSite`**. Chain the methods you
    /// need; a session cookie needs all three flags.
    ///
    /// There is no default `Path`, so set `.path("/")` yourself when
    /// you want the cookie sent for the whole site.
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

    /// A builder that deletes the cookie. It sets an empty value with
    /// `Max-Age=0` and an epoch `Expires`, so the browser drops it.
    ///
    /// `path` must be the path the cookie was set with, and a cookie
    /// set with a `Domain` needs the same `.domain(...)` here too;
    /// the browser matches all three or keeps the cookie.
    #[must_use]
    pub fn deletion(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, "")
            .path(path)
            .max_age(Duration::from_secs(0))
            .expires_at_epoch()
    }

    /// `Path=<path>`: which request paths the browser sends the
    /// cookie on. Most projects want `"/"`.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// `Domain=<domain>`, which widens the cookie to subdomains.
    /// Without it the cookie goes only to the exact host that set
    /// it, so leave it out unless you need the wider scope.
    #[must_use]
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// `Max-Age=<secs>`. A browser prefers it over `Expires` when
    /// both are set.
    #[must_use]
    pub fn max_age(mut self, ttl: Duration) -> Self {
        self.max_age = Some(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX));
        self
    }

    /// `Expires=<http-date>`. Prefer [`Self::max_age`], which is
    /// relative and does not depend on the client clock.
    ///
    /// Pass an RFC 1123 date such as
    /// `"Thu, 01 Jan 1970 00:00:00 GMT"`;
    /// [`crate::http_date::http_date`] formats one for you.
    #[must_use]
    pub fn expires(mut self, http_date_str: impl Into<String>) -> Self {
        self.expires = Some(http_date_str.into());
        self
    }

    /// Set `Expires` to the Unix epoch. [`Self::deletion`] uses it;
    /// call it yourself when you build a delete by hand.
    #[must_use]
    pub fn expires_at_epoch(mut self) -> Self {
        self.expires = Some("Thu, 01 Jan 1970 00:00:00 GMT".to_owned());
        self
    }

    /// Set `HttpOnly`, so JavaScript cannot read the cookie. This
    /// stops an XSS bug from stealing a session. Off by default, so
    /// set it on any cookie the page script does not need.
    #[must_use]
    pub fn http_only(mut self) -> Self {
        self.http_only = true;
        self
    }

    /// Set `Secure`, so the cookie only travels over HTTPS. Off by
    /// default. Set it on anything sensitive, and always when
    /// `SameSite=None`, which browsers reject without it.
    #[must_use]
    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    /// Set `SameSite`, which limits cross-site sending and so blunts
    /// CSRF. See [`SameSite`] for the three values.
    #[must_use]
    pub fn same_site(mut self, value: SameSite) -> Self {
        self.same_site = Some(value);
        self
    }

    /// Render the `Set-Cookie` header value. Attributes come out in
    /// a fixed order: `<name>=<value>; Path; Domain; Max-Age;
    /// Expires; HttpOnly; Secure; SameSite`.
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

    /// Build the header as an `axum::http::HeaderValue`.
    ///
    /// Gives `None` when the rendered string holds a byte a header
    /// may not carry, such as a control character or CR/LF. That is
    /// an `Option` rather than a panic because the value may come
    /// from an attacker, and it also stops response splitting.
    ///
    /// This is the only axum-typed method here, so it alone is
    /// gated; `build()` still works without the feature.
    #[cfg(feature = "_axum")]
    #[must_use]
    pub fn header_value(&self) -> Option<axum::http::HeaderValue> {
        axum::http::HeaderValue::from_str(&self.build()).ok()
    }
}

/// Parse a `Cookie:` header value into a name → value map.
///
/// It splits on `;`, then on the first `=` in each chunk, and trims
/// spaces. A value wrapped in double quotes loses them, per RFC 6265
/// §5.2. A bad chunk is skipped, and the good ones after it still
/// parse.
///
/// Values come from the client, so treat every one as untrusted:
/// check or decode it before you use it.
///
/// Use this outside a request, in tests or a proxy. In an axum
/// handler prefer the `axum-extra` `CookieJar` extractor.
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
            // No `=`: skip the pair and keep decoding the rest.
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let val = val.trim();
        // RFC 6265 §5.2: drop the quotes only when both ends have
        // one. A quote on one end alone stays in the value.
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
        // The shape a deletion starts from.
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
    fn full_attribute_set_renders_in_canonical_order() {
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

    // -------- deletion --------

    #[test]
    fn deletion_shape() {
        // Empty value, Max-Age=0 and an epoch Expires make the
        // browser drop the cookie.
        let s = Cookie::deletion("session", "/").build();
        assert_eq!(
            s,
            "session=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn deletion_with_domain_for_subdomain_scoping() {
        // A browser matches a delete on name, path and domain, so a
        // subdomain cookie needs the same Domain here.
        let s = Cookie::deletion("session", "/")
            .domain(".example.com")
            .build();
        assert!(s.contains("Domain=.example.com"));
        assert!(s.contains("Max-Age=0"));
    }

    // -------- header_value --------
    //
    // Gated like the method they test; the rest of the suite is not.

    #[cfg(feature = "_axum")]
    #[test]
    fn header_value_returns_some_for_normal_cookie() {
        let v = Cookie::new("session", "abc").path("/").header_value();
        assert!(v.is_some());
    }

    #[cfg(feature = "_axum")]
    #[test]
    fn header_value_returns_none_for_invalid_chars() {
        // A NUL byte in the value must not reach a header.
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
        // Quoted at both ends, so both quotes go.
        let m = parse_cookie_header(r#"pref="dark mode""#);
        assert_eq!(m.get("pref"), Some(&"dark mode".to_owned()));
        // Quoted at one end only, so the quote stays.
        let m = parse_cookie_header(r#"x="not closed"#);
        assert_eq!(m.get("x"), Some(&"\"not closed".to_owned()));
    }

    #[test]
    fn parse_cookie_header_empty_input() {
        assert!(parse_cookie_header("").is_empty());
        assert!(parse_cookie_header("   ").is_empty());
        // Semicolons alone hold no chunks.
        assert!(parse_cookie_header(";;;").is_empty());
    }

    #[test]
    fn parse_cookie_header_skips_malformed_chunks() {
        // The chunk with no `=` is skipped, the next one is kept.
        let m = parse_cookie_header("no-equals; good=value");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("good"), Some(&"value".to_owned()));
    }

    #[test]
    fn parse_cookie_header_skips_empty_keys() {
        // `=val` has an empty key, so it is skipped.
        let m = parse_cookie_header("=val; ok=1");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("ok"), Some(&"1".to_owned()));
    }

    #[test]
    fn parse_cookie_header_handles_value_with_equals() {
        // The first `=` splits, so the value may hold more.
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
