//! Named URL reversal, in the shape of Django's `reverse()`.
//!
//! Give a route a stable name with the [`register_url!`] macro, then
//! build the URL again with [`reverse`]:
//!
//! ```ignore
//! use rustango::register_url;
//! use rustango::urls::reverse;
//! use std::collections::HashMap;
//!
//! // At module scope — registers globally via `inventory`.
//! register_url!("post-detail", "/posts/{id}");
//! register_url!("home", "/");
//!
//! // Anywhere at runtime.
//! let url = reverse("home", &HashMap::new()).unwrap();
//! assert_eq!(url, "/");
//!
//! let mut p = HashMap::new();
//! p.insert("id", "42".to_owned());
//! let url = reverse("post-detail", &p).unwrap();
//! assert_eq!(url, "/posts/42");
//! ```
//!
//! Placeholders use axum's `{name}` shape. Values are percent-encoded
//! on substitution, so a value holding `/`, `?` or `#` is still safe.
//! A missing or extra param gives a [`ReverseError`] at call time;
//! there is no compile-time check.
//!
//! **Keep axum in sync yourself.** `register_url!` only adds the
//! pattern to the lookup table. It does not mount a route. Put both
//! next to each other, usually in your own `src/urls.rs`:
//!
//! ```ignore
//! register_url!("post-detail", "/posts/{id}");
//! pub fn router() -> axum::Router {
//!     axum::Router::new().route("/posts/{id}", get(post_detail_view))
//! }
//! ```
//!
//! **Duplicate names** collide in silence: `reverse` returns the first
//! match it finds. Call [`duplicates`] at boot to catch them.
//!
//! ## Namespaced names
//!
//! Django's `reverse("app:detail")` form works as a naming convention.
//! Register a colon-separated name and look it up like any other:
//!
//! ```ignore
//! register_url!("posts:detail", "/posts/{id}");
//! register_url!("users:detail", "/users/{id}");
//!
//! let url = reverse("posts:detail", &p).unwrap(); // → "/posts/42"
//! ```
//!
//! There is no `include()`: every `register_url!` lands in one global
//! registry, so the colon prefix is part of the name you register, not
//! a namespace applied for you.
//!
//! [`register_url!`]: crate::register_url
//! [`reverse`]: crate::urls::reverse
//! [`ReverseError`]: crate::urls::ReverseError
//! [`duplicates`]: crate::urls::duplicates

use std::collections::{HashMap, HashSet};

/// A route pattern with a stable name. Added by
/// [`register_url!`](crate::register_url) at startup, read back by
/// [`reverse`].
#[derive(Debug, Clone)]
pub struct NamedRoute {
    /// The name [`reverse`] and the `{% url %}` tag look up.
    pub name: &'static str,
    /// axum path template; placeholders use `{name}`.
    pub pattern: &'static str,
}

inventory::collect!(NamedRoute);

/// Register a named URL pattern at startup. Uses
/// `rustango::inventory`, so you do not need your own dependency on
/// it.
///
/// ```ignore
/// register_url!("post-detail", "/posts/{id}");
/// ```
#[macro_export]
macro_rules! register_url {
    ($name:expr, $pattern:expr) => {
        $crate::inventory::submit! {
            $crate::urls::NamedRoute {
                name: $name,
                pattern: $pattern,
            }
        }
    };
}

/// Failure modes for [`reverse`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReverseError {
    /// No `register_url!` ran for this name. Either the module holding
    /// it was never loaded, or the name is a typo.
    #[error("no URL registered for name `{0}`")]
    UnknownName(String),

    /// The pattern has a `{param}` placeholder with no value in
    /// `params`.
    #[error("URL `{name}` requires placeholder `{{{param}}}` — pass it in `params`")]
    MissingParam { name: String, param: String },

    /// `params` had a key the pattern does not use. Reported so typos
    /// do not pass unnoticed.
    #[error("URL `{name}` doesn't have a `{{{param}}}` placeholder")]
    UnexpectedParam { name: String, param: String },

    /// The registered pattern is broken, usually an unclosed `{`. It
    /// is a bug in the `register_url!` call, reported here so it can
    /// still be caught.
    #[error("URL `{name}` has a malformed pattern: {detail}")]
    MalformedPattern { name: String, detail: String },
}

/// Every route registered with [`register_url!`](crate::register_url).
/// Useful for boot
/// checks such as [`duplicates`], or an admin page showing the URL
/// map.
#[must_use]
pub fn all_routes() -> Vec<&'static NamedRoute> {
    inventory::iter::<NamedRoute>.into_iter().collect()
}

/// Every name registered more than once, sorted. Empty when the
/// registry is clean. Call it at boot: otherwise a duplicate just
/// means "first one wins", with no warning.
///
/// ```ignore
/// let dups = rustango::urls::duplicates();
/// if !dups.is_empty() {
///     panic!("duplicate URL registrations: {dups:?}");
/// }
/// ```
#[must_use]
pub fn duplicates() -> Vec<&'static str> {
    let mut seen: std::collections::HashMap<&'static str, usize> = std::collections::HashMap::new();
    for r in inventory::iter::<NamedRoute> {
        *seen.entry(r.name).or_insert(0) += 1;
    }
    let mut out: Vec<&'static str> = seen
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect();
    out.sort_unstable();
    out
}

/// Turn a registered name plus params into a URL.
///
/// Each `{name}` placeholder takes the matching value from `params`,
/// percent-encoded, so the result is safe in a `Location` header or a
/// template link. An unused key in `params` is an error, not a
/// silent no-op.
///
/// # Errors
/// - [`ReverseError::UnknownName`] if no route matches `name`.
/// - [`ReverseError::MissingParam`] if a placeholder has no value.
/// - [`ReverseError::UnexpectedParam`] if `params` has an extra key.
pub fn reverse(name: &str, params: &HashMap<&str, String>) -> Result<String, ReverseError> {
    let route = inventory::iter::<NamedRoute>
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| ReverseError::UnknownName(name.to_owned()))?;
    substitute(name, route.pattern, params)
}

/// Same as [`reverse`], but with `String` keys, for JSON and template
/// tags where the keys come from dynamic input.
///
/// # Errors
/// As [`reverse`].
pub fn reverse_owned(name: &str, params: &HashMap<String, String>) -> Result<String, ReverseError> {
    let route = inventory::iter::<NamedRoute>
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| ReverseError::UnknownName(name.to_owned()))?;
    let borrowed: HashMap<&str, String> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    substitute(name, route.pattern, &borrowed)
}

/// Django-parity [`url_has_allowed_host_and_scheme(url, allowed_hosts,
/// require_https=False)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.http.url_has_allowed_host_and_scheme) —
/// returns `true` only when `url` is safe to redirect to, such as the
/// `?next=/path` value on a login or logout view. It blocks the open
/// redirect where attacker input reaches a `Location:` header.
///
/// Rules:
///
/// 1. An empty or whitespace-only URL is unsafe.
/// 2. A control character anywhere (`\0`, `\r`, `\n`, `\t`) is
///    unsafe: those let an attacker smuggle a CRLF header injection.
/// 3. A leading `//`, `\\\\` or `\` is unsafe. Those are
///    protocol-relative or Windows-path shapes that skip the scheme
///    check.
/// 4. Anything else starting with `/` is safe.
/// 5. An absolute URL needs the `http` or `https` scheme, `https`
///    only when `require_https`, and its host must be in
///    `allowed_hosts`. Case is ignored and the port is dropped.
///    Every other scheme is rejected: `javascript:`, `data:`,
///    `file:`, `mailto:` and so on.
///
/// Rules 3 to 5 run twice: once on the URL as given, once with every
/// `\` rewritten to `/`. Browsers do that rewrite themselves, so
/// `/\evil.example/x` passes rule 4 as written but reaches the
/// network as `//evil.example/x`. Both forms have to be safe.
///
/// `allowed_hosts` entries are exact hostnames. There are no
/// wildcards; use `host_validation` for the `.example.com` shape.
///
/// ```ignore
/// use rustango::urls::url_has_allowed_host_and_scheme;
/// // Relative paths always safe.
/// assert!(url_has_allowed_host_and_scheme("/account", &[], false));
/// // Allowed absolute host.
/// assert!(url_has_allowed_host_and_scheme(
///     "https://example.com/x", &["example.com"], false));
/// // Open-redirect attempt rejected.
/// assert!(!url_has_allowed_host_and_scheme(
///     "//evil.com/x", &["example.com"], false));
/// assert!(!url_has_allowed_host_and_scheme(
///     "javascript:alert(1)", &[], false));
/// // Backslash the browser will rewrite to `/` — rejected.
/// assert!(!url_has_allowed_host_and_scheme(
///     "/\\evil.com/x", &["example.com"], false));
/// ```
#[must_use]
pub fn url_has_allowed_host_and_scheme(
    url: &str,
    allowed_hosts: &[&str],
    require_https: bool,
) -> bool {
    // Rule 1: empty or whitespace.
    let trimmed = url.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return false;
    }
    // Rule 2: any control character. Django strips them before
    // parsing; rejecting instead also flags a tampered URL.
    if trimmed.chars().any(|c| c.is_control()) {
        return false;
    }
    // Check the URL as given and as the browser will rewrite it. A
    // backslash can sit inside userinfo, so neither form alone
    // settles it; both must pass.
    if !check_host_and_scheme(trimmed, allowed_hosts, require_https) {
        return false;
    }
    let unescaped = trimmed.replace('\\', "/");
    if unescaped != trimmed && !check_host_and_scheme(&unescaped, allowed_hosts, require_https) {
        return false;
    }
    true
}

/// Rules 3 to 5 of [`url_has_allowed_host_and_scheme`], on a URL that
/// is already trimmed and free of control characters.
fn check_host_and_scheme(trimmed: &str, allowed_hosts: &[&str], require_https: bool) -> bool {
    // Rule 3: protocol-relative or Windows-path injection.
    if trimmed.starts_with("//") || trimmed.starts_with("\\\\") || trimmed.starts_with('\\') {
        return false;
    }
    // Rule 4: relative URLs.
    if trimmed.starts_with('/') {
        return true;
    }
    // Rule 5: absolute URLs. The scheme is parsed by hand to avoid a
    // URL-parser dependency for one helper.
    let Some((scheme_raw, rest)) = trimmed.split_once("://") else {
        // No `://` and no leading `/`: `javascript:foo` or junk.
        return false;
    };
    let scheme = scheme_raw.to_ascii_lowercase();
    let scheme_ok = match scheme.as_str() {
        "https" => true,
        "http" => !require_https,
        _ => false,
    };
    if !scheme_ok {
        return false;
    }
    // The host is everything before the first `/`, `?` or `#`.
    let host_with_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop userinfo: `http://allowed.com@evil.com/` targets evil.com.
    // Split at the LAST `@`, since userinfo can hold one too.
    let host_after_userinfo = host_with_port
        .rsplit_once('@')
        .map_or(host_with_port, |(_, h)| h);
    // Strip port.
    let host = host_after_userinfo
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    allowed_hosts.iter().any(|a| a.to_ascii_lowercase() == host)
}

/// `true` when `url` is absolute: it has a scheme prefix such as
/// `https:`, `mailto:` or `javascript:`, or it starts with `//`.
///
/// Use it as a quick check before treating user input as a same-site
/// path.
///
/// ```ignore
/// use rustango::urls::is_absolute_url;
/// assert!(is_absolute_url("https://example.com/"));
/// assert!(is_absolute_url("mailto:hi@example.com"));
/// assert!(is_absolute_url("//cdn.example.com/img.png"));  // protocol-relative
/// assert!(!is_absolute_url("/account"));
/// assert!(!is_absolute_url("relative/path"));
/// ```
///
/// A scheme is an ASCII letter then any number of `[a-zA-Z0-9+.-]`,
/// then `:` (RFC 3986 §3.1). That covers `http` and `mailto` as well
/// as `javascript`, `data` and `file`. This answers "is it absolute",
/// not "is it safe"; use [`url_has_allowed_host_and_scheme`] for the
/// policy check.
#[must_use]
pub fn is_absolute_url(url: &str) -> bool {
    // `//` plus the backslash spellings a browser rewrites into it:
    // `/\`, `\/` and `\\`. All four leave for another origin, so
    // calling `/\evil.example/x` relative would hand the caller an
    // open redirect.
    let b = url.as_bytes();
    if matches!(b.first(), Some(b'/' | b'\\')) && matches!(b.get(1), Some(b'/' | b'\\')) {
        return true;
    }
    let mut chars = url.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-') {
            return false;
        }
    }
    false
}

/// `true` when `url` is path-relative. The inverse of
/// [`is_absolute_url`].
///
/// ```ignore
/// use rustango::urls::is_relative_url;
/// assert!(is_relative_url("/account"));
/// assert!(is_relative_url("page?x=1"));
/// assert!(!is_relative_url("https://example.com"));
/// assert!(!is_relative_url("//evil.com"));
/// assert!(!is_relative_url("/\\evil.com"));   // browser reads as //
/// ```
///
/// **Not a redirect-safety check.** It only says "this is
/// path-shaped". It ignores control characters, which a browser
/// strips while parsing: `/<TAB>/evil.example` looks relative here and
/// is protocol-relative by the time it is fetched. For a `?next=`
/// target use [`crate::auth_decorators::safe_next`], which the login
/// handlers call.
#[must_use]
pub fn is_relative_url(url: &str) -> bool {
    !is_absolute_url(url)
}

/// Django-parity
/// [`django.utils.http.escape_leading_slashes(url)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.http.escape_leading_slashes) —
/// escape leading slashes and backslashes so a protocol-relative URL
/// cannot become an open redirect.
///
/// A browser reads `Location: //evil.com` as "go to evil.com", since
/// the missing scheme comes from the current page. This escapes a
/// leading `//`, `\\`, `/\` or `\/`, so the URL stays path-relative.
///
/// A URL with none of those prefixes comes back unchanged, so it is
/// cheap to run on every URL as an extra layer.
///
/// ```ignore
/// use rustango::urls::escape_leading_slashes;
///
/// // Safe URLs pass through unchanged.
/// assert_eq!(escape_leading_slashes("/account"), "/account");
/// assert_eq!(escape_leading_slashes("https://example.com/x"),
///            "https://example.com/x");
///
/// // Protocol-relative shapes get the leading slashes escaped.
/// assert_eq!(escape_leading_slashes("//evil.com/path"),
///            "/%2Fevil.com/path");
/// assert_eq!(escape_leading_slashes("\\\\evil.com/path"),
///            "\\%5Cevil.com/path");
/// ```
#[must_use]
pub fn escape_leading_slashes(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        return format!("/%2F{rest}");
    }
    if let Some(rest) = url.strip_prefix("\\\\") {
        return format!("\\%5C{rest}");
    }
    if let Some(rest) = url.strip_prefix("/\\") {
        return format!("/%5C{rest}");
    }
    if let Some(rest) = url.strip_prefix("\\/") {
        return format!("\\%2F{rest}");
    }
    url.to_owned()
}

fn substitute(
    name: &str,
    pattern: &str,
    params: &HashMap<&str, String>,
) -> Result<String, ReverseError> {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut used: HashSet<String> = HashSet::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        // Read to the matching '}'. An unclosed one is a bug in the
        // registered pattern, reported as `MalformedPattern`.
        let mut placeholder = String::new();
        let mut closed = false;
        for nc in chars.by_ref() {
            if nc == '}' {
                closed = true;
                break;
            }
            placeholder.push(nc);
        }
        if !closed {
            return Err(ReverseError::MalformedPattern {
                name: name.to_owned(),
                detail: format!("unclosed placeholder starting at `{{{placeholder}`"),
            });
        }
        // Drop a type annotation like `{int:id}`, so patterns copied
        // from Django routes work as written.
        let key = placeholder.split(':').next_back().unwrap_or(&placeholder);
        let value = params.get(key).ok_or_else(|| ReverseError::MissingParam {
            name: name.to_owned(),
            param: key.to_owned(),
        })?;
        out.push_str(&crate::url_codec::url_encode(value));
        used.insert(key.to_owned());
    }
    // Reject extra params so a typo does not pass unnoticed.
    for k in params.keys() {
        if !used.contains(*k) {
            return Err(ReverseError::UnexpectedParam {
                name: name.to_owned(),
                param: (*k).to_owned(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `__test_` prefix keeps these from clashing with routes
    // another test file in the same binary may register.
    register_url!("__test_home", "/");
    register_url!("__test_post_detail", "/posts/{id}");
    register_url!("__test_two_args", "/users/{user_id}/posts/{post_id}");
    register_url!("__test_typed_placeholder", "/items/{int:id}");
    // Namespaced names: a colon is just part of the name string.
    register_url!("__test_posts:detail", "/posts/{id}");
    register_url!("__test_users:detail", "/users/{id}");

    fn params(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn reverse_resolves_static_pattern() {
        assert_eq!(reverse("__test_home", &HashMap::new()).unwrap(), "/");
    }

    #[test]
    fn reverse_substitutes_single_placeholder() {
        let p = params(&[("id", "42")]);
        assert_eq!(reverse("__test_post_detail", &p).unwrap(), "/posts/42");
    }

    #[test]
    fn reverse_substitutes_multiple_placeholders() {
        let p = params(&[("user_id", "5"), ("post_id", "10")]);
        assert_eq!(reverse("__test_two_args", &p).unwrap(), "/users/5/posts/10");
    }

    #[test]
    fn reverse_percent_encodes_param_values() {
        // A space in a value must not reach the URL raw.
        let p = params(&[("id", "hello world")]);
        let url = reverse("__test_post_detail", &p).unwrap();
        assert_eq!(url, "/posts/hello%20world");
    }

    #[test]
    fn reverse_unknown_name_errors() {
        let err = reverse("nope_doesnt_exist", &HashMap::new()).unwrap_err();
        assert!(matches!(err, ReverseError::UnknownName(ref n) if n == "nope_doesnt_exist"));
    }

    #[test]
    fn reverse_missing_param_errors_with_param_name() {
        let err = reverse("__test_post_detail", &HashMap::new()).unwrap_err();
        match err {
            ReverseError::MissingParam { name, param } => {
                assert_eq!(name, "__test_post_detail");
                assert_eq!(param, "id");
            }
            other => panic!("expected MissingParam, got: {other:?}"),
        }
    }

    #[test]
    fn reverse_unexpected_param_errors() {
        let p = params(&[("id", "1"), ("typo_extra", "x")]);
        let err = reverse("__test_post_detail", &p).unwrap_err();
        assert!(
            matches!(err, ReverseError::UnexpectedParam { ref param, .. } if param == "typo_extra"),
            "got: {err:?}"
        );
    }

    #[test]
    fn reverse_accepts_axum_style_typed_placeholder() {
        // In `/items/{int:id}` the parameter name is `id`.
        let p = params(&[("id", "7")]);
        assert_eq!(reverse("__test_typed_placeholder", &p).unwrap(), "/items/7");
    }

    /// A colon in the name is ordinary text, so `"posts:detail"` is
    /// its own key.
    #[test]
    fn reverse_resolves_namespaced_name() {
        let p = params(&[("id", "1")]);
        assert_eq!(reverse("__test_posts:detail", &p).unwrap(), "/posts/1",);
    }

    /// Two namespaces with the same local name stay apart.
    #[test]
    fn reverse_disambiguates_by_namespace() {
        let p = params(&[("id", "7")]);
        assert_eq!(reverse("__test_posts:detail", &p).unwrap(), "/posts/7",);
        assert_eq!(reverse("__test_users:detail", &p).unwrap(), "/users/7",);
    }

    #[test]
    fn reverse_owned_takes_string_keyed_params() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("id".into(), "99".into());
        assert_eq!(
            reverse_owned("__test_post_detail", &p).unwrap(),
            "/posts/99"
        );
    }

    // Broken on purpose: unclosed `{`.
    register_url!("__test_malformed", "/items/{unclosed");

    #[test]
    fn reverse_malformed_pattern_surfaces_dedicated_error() {
        let err = reverse("__test_malformed", &HashMap::new()).unwrap_err();
        match err {
            ReverseError::MalformedPattern { name, detail } => {
                assert_eq!(name, "__test_malformed");
                assert!(detail.contains("unclosed"), "detail: {detail}");
            }
            other => panic!("expected MalformedPattern, got: {other:?}"),
        }
    }

    #[test]
    fn all_routes_returns_at_least_registered_test_routes() {
        let names: Vec<&str> = all_routes().iter().map(|r| r.name).collect();
        for required in [
            "__test_home",
            "__test_post_detail",
            "__test_two_args",
            "__test_typed_placeholder",
            "__test_malformed",
        ] {
            assert!(names.contains(&required), "missing {required}: {names:?}");
        }
    }

    #[test]
    fn duplicates_helper_is_callable() {
        // Other test files may register routes, so only check that
        // the call works and the result is sorted.
        let dups = duplicates();
        for w in dups.windows(2) {
            assert!(w[0] <= w[1], "duplicates() must return sorted: {dups:?}");
        }
    }
}

// ============================================================== Tera tag

/// Add the `url(...)` Tera function, Django's `{% url %}` in Tera
/// form. Call it once at app setup.
///
/// Tera has no custom `{% tag %}` syntax, only function calls, so the
/// tag becomes a function with keyword arguments:
///
/// ```jinja
/// <a href="{{ url(name='post-detail', id=42) }}">View post</a>
/// ```
///
/// For Django's `{% url 'foo' as my_url %}`, use Tera's `{% set %}`:
///
/// ```jinja
/// {% set my_url = url(name='post-detail', id=42) %}
/// <a href="{{ my_url }}">…</a>
/// ```
///
/// `name` is required and is the registered route name. Every other
/// keyword argument is a path parameter; numbers and bools are
/// stringified and passed to [`reverse_owned`].
///
/// A [`reverse`] error becomes a Tera render error, which the
/// template views turn into a 500.
#[cfg(feature = "template_views")]
pub fn register_url_tag(tera: &mut tera::Tera) {
    tera.register_function("url", url_tag_fn);
}

#[cfg(feature = "template_views")]
fn url_tag_fn(args: &std::collections::HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    let name = match args.get("name") {
        Some(tera::Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(tera::Error::msg(format!(
                "url(): `name` must be a string, got: {other:?}"
            )));
        }
        None => return Err(tera::Error::msg("url(): missing required `name` argument")),
    };
    // Every other arg is a path parameter. Scalars are stringified;
    // objects and arrays are rejected.
    let mut params: HashMap<String, String> = HashMap::new();
    for (k, v) in args {
        if k == "name" {
            continue;
        }
        let s = match v {
            tera::Value::String(s) => s.clone(),
            tera::Value::Number(n) => n.to_string(),
            tera::Value::Bool(b) => b.to_string(),
            tera::Value::Null => {
                // Null nearly always means an undefined template
                // variable. Emitting `/posts/` would hide the bug.
                return Err(tera::Error::msg(format!(
                    "url(): argument `{k}` is null — likely an undefined template variable"
                )));
            }
            other => {
                return Err(tera::Error::msg(format!(
                    "url(): argument `{k}` must be a scalar (string / number / bool), got: {other:?}"
                )));
            }
        };
        params.insert(k.clone(), s);
    }
    reverse_owned(&name, &params)
        .map(tera::Value::String)
        .map_err(|e| tera::Error::msg(e.to_string()))
}

#[cfg(all(test, feature = "template_views"))]
mod tera_tests {
    use super::*;

    register_url!("__test_tag_home", "/");
    register_url!("__test_tag_post", "/posts/{id}");
    register_url!("__test_tag_users_posts", "/users/{user_id}/posts/{post_id}");

    fn setup() -> tera::Tera {
        let mut tera = tera::Tera::default();
        register_url_tag(&mut tera);
        tera
    }

    fn render(tera: &tera::Tera, src: &str) -> String {
        let mut t = tera.clone();
        t.add_raw_template("_", src).unwrap();
        t.render("_", &tera::Context::new()).unwrap()
    }

    #[test]
    fn url_tag_resolves_static_route() {
        let tera = setup();
        assert_eq!(render(&tera, "{{ url(name='__test_tag_home') }}"), "/");
    }

    #[test]
    fn url_tag_substitutes_int_param_via_display() {
        // Tera passes `42` as a number; `url()` stringifies it.
        let tera = setup();
        assert_eq!(
            render(&tera, "{{ url(name='__test_tag_post', id=42) }}"),
            "/posts/42"
        );
    }

    #[test]
    fn url_tag_substitutes_string_param() {
        let tera = setup();
        assert_eq!(
            render(&tera, "{{ url(name='__test_tag_post', id='hello') }}"),
            "/posts/hello"
        );
    }

    #[test]
    fn url_tag_substitutes_multiple_params() {
        let tera = setup();
        assert_eq!(
            render(
                &tera,
                "{{ url(name='__test_tag_users_posts', user_id=5, post_id=10) }}"
            ),
            "/users/5/posts/10"
        );
    }

    #[test]
    fn url_tag_set_capture_works_via_tera_set() {
        // Django's `{% url 'foo' as bar %}`, written with Tera's
        // `{% set %}`.
        let tera = setup();
        let src = "{% set u = url(name='__test_tag_post', id=7) %}<a href='{{ u }}'>x</a>";
        assert_eq!(render(&tera, src), "<a href='/posts/7'>x</a>");
    }

    /// Flatten a Tera error's `source` chain into one string. The
    /// outer error only says "Failed to render '_'"; the real cause
    /// sits on `e.source()`.
    fn full_error_chain(e: &tera::Error) -> String {
        use std::error::Error as _;
        let mut out = format!("{e}");
        let mut cur: Option<&dyn std::error::Error> = e.source();
        while let Some(c) = cur {
            out.push_str(" | ");
            out.push_str(&c.to_string());
            cur = c.source();
        }
        out
    }

    #[test]
    fn url_tag_missing_name_arg_errors() {
        let mut tera = setup();
        tera.add_raw_template("_", "{{ url(id=1) }}").unwrap();
        let err = tera.render("_", &tera::Context::new()).unwrap_err();
        let msg = full_error_chain(&err).to_lowercase();
        assert!(
            msg.contains("name") || msg.contains("url()"),
            "expected error about missing `name`, got: {msg}"
        );
    }

    #[test]
    fn url_tag_unknown_route_propagates_reverse_error() {
        let mut tera = setup();
        tera.add_raw_template("_", "{{ url(name='nope_nope_nope') }}")
            .unwrap();
        let err = tera.render("_", &tera::Context::new()).unwrap_err();
        let msg = full_error_chain(&err).to_lowercase();
        assert!(
            msg.contains("no url registered") || msg.contains("nope_nope_nope"),
            "expected unknown-name error, got: {msg}"
        );
    }

    #[test]
    fn url_tag_non_string_name_errors_clearly() {
        let mut tera = setup();
        tera.add_raw_template("_", "{{ url(name=42) }}").unwrap();
        let err = tera.render("_", &tera::Context::new()).unwrap_err();
        let msg = full_error_chain(&err).to_lowercase();
        assert!(
            msg.contains("name") && msg.contains("string"),
            "expected `name must be a string` error, got: {msg}"
        );
    }

    #[test]
    fn url_tag_null_param_errors_instead_of_emitting_empty_segment() {
        // A JSON null in the context must not become `/posts/`.
        let mut tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("v", &serde_json::Value::Null);
        tera.add_raw_template("_", "{{ url(name='__test_tag_post', id=v) }}")
            .unwrap();
        let err = tera.render("_", &ctx).unwrap_err();
        let msg = full_error_chain(&err).to_lowercase();
        assert!(
            msg.contains("null") || msg.contains("undefined"),
            "expected null/undefined error, got: {msg}"
        );
    }
}

// ============================================================== querystring filter

/// Add the `querystring` Tera filter, Django's `{% querystring %}` in
/// Tera form. It takes the current query string and returns it with
/// your overrides applied.
///
/// Use it for pagination and filter links, where you want the same
/// URL with `page=3` instead of `page=2`:
///
/// ```jinja
/// <!-- request.query_string = "q=hello&page=1&sort=asc" -->
/// <a href="?{{ request.query_string | querystring(page=2) | safe }}">page 2</a>
/// <!--                                       ↑ → "?q=hello&page=2&sort=asc" -->
/// ```
///
/// It follows Django's [`{% querystring %}`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#querystring):
///
/// - An override replaces the key if it is there, or appends it. One
///   value per key.
/// - A `null` value removes the key: `{{ qs | querystring(filter=null) }}`.
/// - An empty result is the empty string, with no dangling `?`.
/// - Keys and values are percent-encoded with
///   [`crate::url_codec::url_encode`].
///
/// Wire it once at app setup:
///
/// ```ignore
/// rustango::urls::register_querystring_filter(&mut tera);
/// ```
#[cfg(feature = "template_views")]
pub fn register_querystring_filter(tera: &mut tera::Tera) {
    tera.register_filter("querystring", querystring_filter);
}

#[cfg(feature = "template_views")]
fn querystring_filter(
    value: &tera::Value,
    args: &HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let current = value.as_str().unwrap_or("");
    let mut pairs = parse_query_pairs(current);

    for (k, v) in args {
        if matches!(v, tera::Value::Null) {
            // Null means drop every copy of this key.
            pairs.retain(|(pk, _)| pk != k);
            continue;
        }
        let s = match v {
            tera::Value::String(s) => s.clone(),
            tera::Value::Number(n) => n.to_string(),
            tera::Value::Bool(b) => b.to_string(),
            other => {
                return Err(tera::Error::msg(format!(
                    "querystring(): argument `{k}` must be a scalar (string / number / bool / null), got: {other:?}"
                )));
            }
        };
        // Django's `QueryDict.__setitem__`: replace an existing key
        // in place, keep its position, and collapse duplicates into
        // one. A new key goes at the end.
        let mut found = false;
        let mut i = 0;
        while i < pairs.len() {
            if pairs[i].0 == *k {
                if found {
                    // The first slot already took the override.
                    pairs.remove(i);
                    continue;
                }
                pairs[i].1 = s.clone();
                found = true;
            }
            i += 1;
        }
        if !found {
            pairs.push((k.clone(), s));
        }
    }

    if pairs.is_empty() {
        return Ok(tera::Value::String(String::new()));
    }
    let encoded: Vec<String> = pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                crate::url_codec::url_encode(k),
                crate::url_codec::url_encode(v)
            )
        })
        .collect();
    Ok(tera::Value::String(format!("?{}", encoded.join("&"))))
}

/// Parse a query string into `(key, value)` pairs, in order. A
/// leading `?` is dropped, so either form of input works.
///
/// Odd pairs are read the loose way browsers do: a chunk with no `=`
/// gets an empty value, and a chunk with several `=` splits on the
/// first one.
///
/// ```ignore
/// use rustango::urls::parse_query_pairs;
/// assert_eq!(
///     parse_query_pairs("q=hello&page=2"),
///     vec![("q".to_owned(), "hello".to_owned()),
///          ("page".to_owned(), "2".to_owned())]
/// );
/// // Percent-encoded values get decoded.
/// assert_eq!(
///     parse_query_pairs("q=hello%20world"),
///     vec![("q".to_owned(), "hello world".to_owned())]
/// );
/// // Multi-value: same key appears twice — both pairs kept (Django shape).
/// assert_eq!(
///     parse_query_pairs("tag=a&tag=b"),
///     vec![("tag".to_owned(), "a".to_owned()),
///          ("tag".to_owned(), "b".to_owned())]
/// );
/// ```
#[must_use]
pub fn parse_query_pairs(s: &str) -> Vec<(String, String)> {
    let s = s.trim_start_matches('?');
    if s.is_empty() {
        return Vec::new();
    }
    s.split('&')
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| match chunk.split_once('=') {
            Some((k, v)) => (
                crate::url_codec::url_decode(k),
                crate::url_codec::url_decode(v),
            ),
            None => (crate::url_codec::url_decode(chunk), String::new()),
        })
        .collect()
}

/// Like [`parse_query_pairs`], but grouped: every value for a key
/// lands in one vec, so you can ask for all values of key X at once.
///
/// ```ignore
/// use rustango::urls::parse_query_pairs_grouped;
/// let m = parse_query_pairs_grouped("tag=a&tag=b&q=hello");
/// assert_eq!(m.get("tag"), Some(&vec!["a".to_owned(), "b".to_owned()]));
/// assert_eq!(m.get("q"), Some(&vec!["hello".to_owned()]));
/// ```
#[must_use]
pub fn parse_query_pairs_grouped(s: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (k, v) in parse_query_pairs(s) {
        out.entry(k).or_default().push(v);
    }
    out
}

#[cfg(all(test, feature = "template_views"))]
mod querystring_tests {
    use super::*;

    fn setup() -> tera::Tera {
        let mut tera = tera::Tera::default();
        register_querystring_filter(&mut tera);
        tera
    }

    fn render(tera: &tera::Tera, src: &str, ctx: tera::Context) -> String {
        let mut t = tera.clone();
        t.add_raw_template("_", src).unwrap();
        t.render("_", &ctx).unwrap()
    }

    #[test]
    fn empty_input_with_overrides_emits_new_qs() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "");
        assert_eq!(
            render(&tera, "{{ q | querystring(page=2) | safe }}", ctx),
            "?page=2"
        );
    }

    #[test]
    fn empty_input_with_no_overrides_emits_empty_string() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "");
        assert_eq!(render(&tera, "{{ q | querystring() | safe }}", ctx), "");
    }

    #[test]
    fn override_replaces_existing_key() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "page=1");
        assert_eq!(
            render(&tera, "{{ q | querystring(page=2) | safe }}", ctx),
            "?page=2"
        );
    }

    #[test]
    fn override_preserves_other_keys_and_position() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "q=hello&page=1&sort=asc");
        let out = render(&tera, "{{ q | querystring(page=2) | safe }}", ctx);
        // `page` keeps its place between `q` and `sort`: the value
        // changes, the key does not move to the end.
        assert_eq!(out, "?q=hello&page=2&sort=asc");
    }

    #[test]
    fn override_collapses_duplicate_existing_keys() {
        // An override replaces the whole value list, so the
        // duplicates collapse into one pair.
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "tag=a&tag=b&tag=c");
        assert_eq!(
            render(&tera, "{{ q | querystring(tag='x') | safe }}", ctx),
            "?tag=x"
        );
    }

    #[test]
    fn override_appends_new_key() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "q=hello");
        assert_eq!(
            render(&tera, "{{ q | querystring(filter='active') | safe }}", ctx),
            "?q=hello&filter=active"
        );
    }

    #[test]
    fn null_override_removes_key() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "q=hello&filter=active");
        ctx.insert("v", &serde_json::Value::Null);
        assert_eq!(
            render(&tera, "{{ q | querystring(filter=v) | safe }}", ctx),
            "?q=hello"
        );
    }

    #[test]
    fn percent_encodes_special_chars_in_output() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "");
        let out = render(
            &tera,
            "{{ q | querystring(name='hello world', special='a/b?c') | safe }}",
            ctx,
        );
        // `' '` → `%20`, `/` → `%2F`, `?` → `%3F`.
        assert!(out.contains("hello%20world"), "got: {out}");
        assert!(out.contains("a%2Fb%3Fc"), "got: {out}");
    }

    #[test]
    fn input_with_leading_question_mark_is_stripped() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "?q=hello&page=1");
        assert_eq!(
            render(&tera, "{{ q | querystring(page=2) | safe }}", ctx),
            "?q=hello&page=2"
        );
    }

    #[test]
    fn bool_and_number_args_stringify_via_display() {
        let tera = setup();
        let mut ctx = tera::Context::new();
        ctx.insert("q", "");
        let out = render(
            &tera,
            "{{ q | querystring(page=2, active=true) | safe }}",
            ctx,
        );
        assert!(out.contains("page=2"), "got: {out}");
        assert!(out.contains("active=true"), "got: {out}");
    }

    #[test]
    fn parse_pairs_handles_trailing_ampersand_and_empty_chunks() {
        // Browsers sometimes send `?q=x&`, so empty chunks drop out.
        let pairs = parse_query_pairs("?q=hello&&page=1&");
        assert_eq!(
            pairs,
            vec![
                ("q".to_owned(), "hello".to_owned()),
                ("page".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_pairs_percent_decodes_keys_and_values() {
        let pairs = parse_query_pairs("q=hello%20world&page=1");
        assert_eq!(pairs[0].1, "hello world");
    }

    // ---------- url_has_allowed_host_and_scheme (Django parity) ----------

    #[test]
    fn safe_url_accepts_relative_paths() {
        assert!(url_has_allowed_host_and_scheme("/account", &[], false));
        assert!(url_has_allowed_host_and_scheme(
            "/dashboard?next=/x",
            &[],
            false
        ));
        assert!(url_has_allowed_host_and_scheme("/", &[], false));
    }

    #[test]
    fn safe_url_accepts_allowed_host() {
        assert!(url_has_allowed_host_and_scheme(
            "https://example.com/x",
            &["example.com"],
            false
        ));
        assert!(url_has_allowed_host_and_scheme(
            "http://example.com/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_protocol_relative() {
        // The classic open redirect: `//evil.com/path` is
        // scheme-relative and slips past a plain `starts_with('/')`.
        assert!(!url_has_allowed_host_and_scheme(
            "//evil.com/x",
            &["example.com"],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme(
            "//attacker.com",
            &[],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_windows_path_injection() {
        assert!(!url_has_allowed_host_and_scheme(
            "\\\\evil.com\\x",
            &["example.com"],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme(
            "\\backslash\\path",
            &[],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_javascript_scheme() {
        assert!(!url_has_allowed_host_and_scheme(
            "javascript:alert(1)",
            &[],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme(
            "JaVaScRiPt:alert(1)",
            &[],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_data_and_file_schemes() {
        assert!(!url_has_allowed_host_and_scheme(
            "data:text/html,<script>...",
            &[],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme(
            "file:///etc/passwd",
            &[],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_disallowed_host() {
        // example.com is allowed, but the URL goes somewhere else.
        assert!(!url_has_allowed_host_and_scheme(
            "https://evil.com/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_backslash_the_browser_rewrites_to_slash() {
        // Browsers rewrite `\` to `/`, so each of these leaves as
        // `//evil.com/x` even though the text starts with `/`.
        for url in ["/\\evil.com/x", "/\\\\evil.com/x", "\\/evil.com/x"] {
            assert!(
                !url_has_allowed_host_and_scheme(url, &["example.com"], false),
                "{url} must be rejected: the browser sends it to evil.com",
            );
        }
    }

    #[test]
    fn safe_url_still_accepts_ordinary_relative_paths() {
        // Control for the test above: the backslash check must not
        // reject paths that were always safe.
        for url in ["/account", "/a/b?q=1&r=2", "/x#frag"] {
            assert!(
                url_has_allowed_host_and_scheme(url, &["example.com"], false),
                "{url} is an ordinary relative path and must stay safe",
            );
        }
    }

    #[test]
    fn safe_url_rejects_userinfo_smuggling() {
        // In `http://example.com@evil.com/` the host is evil.com,
        // even though a quick read sees example.com.
        assert!(!url_has_allowed_host_and_scheme(
            "http://example.com@evil.com/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_strips_port_before_comparing() {
        // `example.com:8080` matches the `example.com` entry.
        assert!(url_has_allowed_host_and_scheme(
            "https://example.com:8080/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_case_insensitive_host_match() {
        assert!(url_has_allowed_host_and_scheme(
            "https://Example.COM/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_require_https_blocks_http() {
        assert!(!url_has_allowed_host_and_scheme(
            "http://example.com/x",
            &["example.com"],
            true
        ));
        assert!(url_has_allowed_host_and_scheme(
            "https://example.com/x",
            &["example.com"],
            true
        ));
    }

    #[test]
    fn safe_url_rejects_empty_and_whitespace() {
        assert!(!url_has_allowed_host_and_scheme("", &[], false));
        assert!(!url_has_allowed_host_and_scheme("   ", &[], false));
    }

    #[test]
    fn safe_url_rejects_embedded_control_chars() {
        // CRLF and null injection smuggle header lines into a
        // Location redirect.
        assert!(!url_has_allowed_host_and_scheme(
            "/path\nLocation: //evil.com",
            &[],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme(
            "/path\r\nX-Inject: y",
            &[],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme("/path\0", &[], false));
    }

    #[test]
    fn safe_url_rejects_malformed_scheme() {
        // No `://`, so neither absolute nor relative.
        assert!(!url_has_allowed_host_and_scheme(
            "mailto:foo@bar.com",
            &[],
            false
        ));
        assert!(!url_has_allowed_host_and_scheme(
            "vbscript:msgbox(1)",
            &[],
            false
        ));
    }

    // ---------- parse_query_pairs / parse_query_pairs_grouped (Django parity) ----------

    #[test]
    fn parse_qs_basic() {
        assert_eq!(
            parse_query_pairs("q=hello&page=2"),
            vec![
                ("q".to_owned(), "hello".to_owned()),
                ("page".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_qs_strips_leading_question_mark() {
        assert_eq!(
            parse_query_pairs("?a=1&b=2"),
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_qs_empty_returns_empty_vec() {
        assert!(parse_query_pairs("").is_empty());
        assert!(parse_query_pairs("?").is_empty());
    }

    #[test]
    fn parse_qs_percent_decodes_keys_and_values() {
        assert_eq!(
            parse_query_pairs("q=hello%20world"),
            vec![("q".to_owned(), "hello world".to_owned())]
        );
    }

    #[test]
    fn parse_qs_multi_value_preserves_all_pairs() {
        assert_eq!(
            parse_query_pairs("tag=a&tag=b"),
            vec![
                ("tag".to_owned(), "a".to_owned()),
                ("tag".to_owned(), "b".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_qs_handles_key_without_value() {
        // `flag` is a bare key with no `=`.
        assert_eq!(
            parse_query_pairs("flag&q=hi"),
            vec![
                ("flag".to_owned(), String::new()),
                ("q".to_owned(), "hi".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_qs_grouped_collects_multi_values() {
        let m = parse_query_pairs_grouped("tag=a&tag=b&q=hello");
        assert_eq!(m.get("tag"), Some(&vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(m.get("q"), Some(&vec!["hello".to_owned()]));
    }

    #[test]
    fn parse_qs_grouped_empty_returns_empty_map() {
        assert!(parse_query_pairs_grouped("").is_empty());
    }

    // ---------- is_absolute_url / is_relative_url ----------

    #[test]
    fn is_absolute_url_recognizes_https_and_mailto() {
        assert!(is_absolute_url("https://example.com/"));
        assert!(is_absolute_url("http://example.com"));
        assert!(is_absolute_url("mailto:hi@example.com"));
        assert!(is_absolute_url("ftp://files.example.org/"));
    }

    #[test]
    fn is_absolute_url_flags_protocol_relative() {
        assert!(is_absolute_url("//cdn.example.com/img.png"));
    }

    #[test]
    fn is_absolute_url_flags_dangerous_schemes() {
        // "Absolute" is not "safe": a dangerous scheme is still
        // absolute.
        assert!(is_absolute_url("javascript:alert(1)"));
        assert!(is_absolute_url("data:text/html,<x>"));
        assert!(is_absolute_url("file:///etc/passwd"));
    }

    #[test]
    fn is_absolute_url_rejects_relative_paths() {
        assert!(!is_absolute_url("/account"));
        assert!(!is_absolute_url("page"));
        assert!(!is_absolute_url("page?x=1"));
        assert!(!is_absolute_url("/path:with-colon"));
        assert!(!is_absolute_url(""));
    }

    #[test]
    fn is_relative_url_is_inverse_of_absolute() {
        assert!(is_relative_url("/account"));
        assert!(is_relative_url(""));
        assert!(!is_relative_url("https://example.com"));
        assert!(!is_relative_url("//evil.com"));
    }

    #[test]
    fn is_absolute_url_rejects_invalid_scheme_chars() {
        // A `_` is not legal in a scheme.
        assert!(!is_absolute_url("not_a_scheme:value"));
    }

    // ---------- escape_leading_slashes (Django parity) ----------

    #[test]
    fn escape_leading_slashes_passes_through_safe_urls() {
        assert_eq!(escape_leading_slashes("/account"), "/account");
        assert_eq!(escape_leading_slashes("/"), "/");
        assert_eq!(escape_leading_slashes(""), "");
        assert_eq!(escape_leading_slashes("plain-text"), "plain-text");
        assert_eq!(
            escape_leading_slashes("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn escape_leading_slashes_escapes_protocol_relative() {
        // With no scheme, a browser reads `//evil.com` as
        // `https://evil.com`.
        assert_eq!(
            escape_leading_slashes("//evil.com/path"),
            "/%2Fevil.com/path"
        );
    }

    #[test]
    fn escape_leading_slashes_escapes_backslash_pairs() {
        assert_eq!(
            escape_leading_slashes("\\\\evil.com/path"),
            "\\%5Cevil.com/path"
        );
    }

    #[test]
    fn escape_leading_slashes_escapes_mixed_pairs() {
        // Some browser parsers confuse `/\` and `\/` with `//`.
        assert_eq!(escape_leading_slashes("/\\evil"), "/%5Cevil");
        assert_eq!(escape_leading_slashes("\\/evil"), "\\%2Fevil");
    }

    #[test]
    fn escape_leading_slashes_only_touches_the_first_two_chars() {
        // A `//` further in stays as is; only the prefix matters.
        assert_eq!(
            escape_leading_slashes("/path//slashes//inside"),
            "/path//slashes//inside"
        );
    }
}
