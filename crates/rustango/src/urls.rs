//! Named URL reversal — Django's `reverse()` + `get_absolute_url()`.
//! Issue #8.
//!
//! Routes can be registered with a stable name at module-load time
//! via the [`register_url!`] macro, then resolved back to a URL string
//! through [`reverse`]. The shape mirrors Django's URL conf:
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
//! Path-template placeholders use axum 0.8's `{name}` shape. Param
//! values are percent-encoded on substitution so caller-supplied
//! values are safe even when they contain `/` / `?` / `#`. Missing
//! or extra params return a [`ReverseError`] at call time — there's
//! no compile-time check today (would require either codegen or a
//! shared sentinel; queued as a future enhancement).
//!
//! **Manual sync with axum routes**: `register_url!` only registers
//! the *pattern string* with the reverse-lookup table — it does NOT
//! mount a route on any axum `Router`. Callers must keep their
//! axum routing in sync with their `register_url!` calls. A common
//! shape is to put both side-by-side in `src/urls.rs`:
//!
//! ```ignore
//! register_url!("post-detail", "/posts/{id}");
//! pub fn router() -> axum::Router {
//!     axum::Router::new().route("/posts/{id}", get(post_detail_view))
//! }
//! ```
//!
//! **Duplicate names**: two `register_url!("name", ...)` calls in the
//! same binary collide silently — `reverse` picks the first match it
//! finds in the inventory. Call [`duplicates`] at boot to surface any
//! name registered more than once. Future work: a startup validator
//! that turns this into a clean error before the server binds.
//!
//! ## Namespaced reverse (Django parity #380)
//!
//! Django's `reverse("app:detail")` form works today as a naming
//! convention — register routes with colon-separated names and they
//! look up just like any other key:
//!
//! ```ignore
//! register_url!("posts:detail", "/posts/{id}");
//! register_url!("users:detail", "/users/{id}");
//!
//! let url = reverse("posts:detail", &p).unwrap(); // → "/posts/42"
//! ```
//!
//! Rustango has no `include()` concept (every `register_url!` runs
//! at module-load and lands in the global registry via `inventory`),
//! so the colon prefix is part of the registered name rather than
//! an auto-applied namespace. The behavior matches Django's
//! `reverse("app:detail")` API surface.

use std::collections::{HashMap, HashSet};

/// A named route — pattern + stable name. Registered at static-init
/// time via [`register_url!`] (which calls `inventory::submit!`
/// under the hood) and looked up by [`reverse`] at runtime.
#[derive(Debug, Clone)]
pub struct NamedRoute {
    /// Stable name used by [`reverse`] / Django's `{% url %}`.
    pub name: &'static str,
    /// axum 0.8 path template — placeholders use `{name}` syntax.
    pub pattern: &'static str,
}

inventory::collect!(NamedRoute);

/// Register a named URL pattern at module-load time. Picks up
/// `rustango::inventory` so callers don't need their own dep.
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
    /// No `register_url!("name", ...)` has run for this name. Either
    /// the registration is in a module that wasn't loaded, or the
    /// name is a typo.
    #[error("no URL registered for name `{0}`")]
    UnknownName(String),

    /// The pattern has a `{param}` placeholder but `params` didn't
    /// include a value for it.
    #[error("URL `{name}` requires placeholder `{{{param}}}` — pass it in `params`")]
    MissingParam { name: String, param: String },

    /// `params` had keys that don't appear in the pattern. Surfaced
    /// to catch typos that would otherwise silently disappear.
    #[error("URL `{name}` doesn't have a `{{{param}}}` placeholder")]
    UnexpectedParam { name: String, param: String },

    /// The registered pattern is malformed — most commonly an
    /// unclosed `{` placeholder. Programmer bug at `register_url!`
    /// time; surfaces from `reverse` so it's at least catchable.
    #[error("URL `{name}` has a malformed pattern: {detail}")]
    MalformedPattern { name: String, detail: String },
}

/// Snapshot of the route registry — every entry registered via
/// [`register_url!`] across every loaded module. Useful for boot-time
/// diagnostics (see [`duplicates`]) or admin endpoints that surface
/// the URL map.
#[must_use]
pub fn all_routes() -> Vec<&'static NamedRoute> {
    inventory::iter::<NamedRoute>.into_iter().collect()
}

/// List every name that's been registered more than once. Empty
/// vec on a clean registry. Call at app boot — duplicates surface
/// as silent "first wins" otherwise.
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

/// Resolve a registered name + parameters into a concrete URL string.
///
/// Substitutes every `{name}` placeholder in the pattern with the
/// corresponding entry in `params`, percent-encoding the value so the
/// resulting URL is safe to use as a `Location` header or template
/// link. Extra `params` keys that don't appear in the pattern surface
/// as [`ReverseError::UnexpectedParam`] — strict by default so typos
/// don't disappear.
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

/// Same as [`reverse`] but with `String` keys — convenience for the
/// JSON / template-tag path where keys come from dynamic input.
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
/// returns `true` only when `url` is safe to redirect to from a
/// trusted handler (e.g. the `?next=/path` parameter on
/// `LoginView` / `LogoutView`). Defends against the open-redirect
/// vector where attacker-controlled input lands in a `Location:`
/// header pointing at an external site.
///
/// Rules (matching Django 6.0):
///
/// 1. Empty / whitespace-only URLs are unsafe.
/// 2. Reject control characters (`\0`, `\r`, `\n`, `\t`) anywhere
///    in the URL — they short-circuit parsers and let attackers
///    smuggle CRLF header injections.
/// 3. Reject leading `//` or `\\\\` (protocol-relative URLs that
///    bypass the scheme check) and leading `\` (Windows path
///    injection).
/// 4. Relative URLs (starting with `/` after the control-char
///    strip) are always safe.
/// 5. Absolute URLs must use `http` or `https` (case-insensitive);
///    if `require_https`, only `https` is accepted. The host
///    must appear in `allowed_hosts` (case-insensitive, port
///    stripped). Reject every other scheme — `javascript:`,
///    `data:`, `file:`, `mailto:`, `vbscript:`, etc.
///
/// `allowed_hosts` entries are exact hostnames — no wildcard
/// support (Django doesn't either; pair with `host_validation`
/// for `.example.com` shape).
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
/// ```
#[must_use]
pub fn url_has_allowed_host_and_scheme(
    url: &str,
    allowed_hosts: &[&str],
    require_https: bool,
) -> bool {
    // Rule 1 — empty / whitespace.
    let trimmed = url.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return false;
    }
    // Rule 2 — reject control characters anywhere. Django strips
    // them BEFORE parsing; we reject so the function is also useful
    // as a "this URL was tampered with" signal.
    if trimmed.chars().any(|c| c.is_control()) {
        return false;
    }
    // Rule 3 — protocol-relative / Windows-path injection.
    if trimmed.starts_with("//") || trimmed.starts_with("\\\\") || trimmed.starts_with('\\') {
        return false;
    }
    // Rule 4 — relative URLs.
    if trimmed.starts_with('/') {
        return true;
    }
    // Rule 5 — absolute URLs. Parse scheme manually so we don't pull
    // a URL parser dep just for this one helper.
    let Some((scheme_raw, rest)) = trimmed.split_once("://") else {
        // No `://` and didn't start with `/` — could be `javascript:foo`
        // or a malformed input. Reject.
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
    // Host = everything before the first `/`, `?`, or `#`.
    let host_with_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip userinfo if present (attacker shape: `http://allowed.com@evil.com/`).
    // Django strips at the LAST `@` to handle nested userinfo;
    // post-`@` is the real host.
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

/// `true` when `url` looks absolute — it carries a scheme prefix
/// (`https:` / `mailto:` / `javascript:` / etc.) OR starts with `//`
/// (protocol-relative).
///
/// Use this as a quick check before treating user-provided URL
/// input as a same-site path. The complement is "path-relative",
/// the usual safe shape for `Location:` redirects to your own app.
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
/// Recognized scheme shape: ASCII letter followed by 0+
/// `[a-zA-Z0-9+.-]` characters then `:` (RFC 3986 §3.1). Matches
/// both registered schemes (`http`, `https`, `mailto`, `ftp`, …)
/// and exotic / dangerous ones (`javascript`, `data`, `file`, …) —
/// the predicate is "is absolute," not "is safe." Pair with
/// [`url_has_allowed_host_and_scheme`] for the policy check.
#[must_use]
pub fn is_absolute_url(url: &str) -> bool {
    if url.starts_with("//") {
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

/// `true` when `url` is path-relative (the inverse of
/// [`is_absolute_url`]). Useful as a positive-frame predicate:
///
/// ```ignore
/// use rustango::urls::is_relative_url;
/// assert!(is_relative_url("/account"));
/// assert!(is_relative_url("page?x=1"));
/// assert!(!is_relative_url("https://example.com"));
/// assert!(!is_relative_url("//evil.com"));
/// ```
#[must_use]
pub fn is_relative_url(url: &str) -> bool {
    !is_absolute_url(url)
}

/// Django-parity
/// [`django.utils.http.escape_leading_slashes(url)`](https://docs.djangoproject.com/en/6.0/ref/utils/#django.utils.http.escape_leading_slashes) —
/// escape leading slashes / backslashes on a URL fragment to prevent
/// open-redirect attacks via protocol-relative URLs.
///
/// A `Location: //evil.com` header is interpreted by the browser as
/// "navigate to https://evil.com" (the absent scheme defaults to the
/// current page's). This helper escapes the leading `//` / `\\` /
/// `/\` / `\/` to `/%2F` / `\%5C` / etc., so the rendered URL stays
/// path-relative.
///
/// Returns the URL unchanged when it doesn't start with one of the
/// risky double-prefix shapes — caller can use it as a defense-in-
/// depth pass without a length penalty on every URL.
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
        // Read until the matching '}'. An unclosed placeholder is a
        // programmer bug in the registered pattern — surface it
        // cleanly via `MalformedPattern`.
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
        // Strip axum-style / Django-style type annotations like
        // `{id:int}` — accept both `name` and `type:name` shapes so
        // patterns ported from Django routes work as-is.
        let key = placeholder.split(':').next_back().unwrap_or(&placeholder);
        let value = params.get(key).ok_or_else(|| ReverseError::MissingParam {
            name: name.to_owned(),
            param: key.to_owned(),
        })?;
        out.push_str(&crate::url_codec::url_encode(value));
        used.insert(key.to_owned());
    }
    // Reject extra params (typo-safety).
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

    // Registered at module-init time via the macro. Names are
    // prefixed `__test_` to keep collision risk low if other test
    // files in the binary ever register routes too.
    register_url!("__test_home", "/");
    register_url!("__test_post_detail", "/posts/{id}");
    register_url!("__test_two_args", "/users/{user_id}/posts/{post_id}");
    register_url!("__test_typed_placeholder", "/items/{int:id}");
    // Issue #380 — Django-shape namespaced names. Colons in names
    // round-trip cleanly through register_url! + reverse, so the
    // namespace convention is supported without any new code.
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
        // `/` in a value must NOT escape the path segment.
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
        // Pattern `/items/{int:id}` — `id` is the parameter name.
        let p = params(&[("id", "7")]);
        assert_eq!(reverse("__test_typed_placeholder", &p).unwrap(), "/items/7");
    }

    /// Issue #380 — Django-shape namespaced names round-trip through
    /// `register_url!` + `reverse`. Colons in the name are part of
    /// the name string (no special syntax), so `"posts:detail"` is
    /// resolved as a unique key.
    #[test]
    fn reverse_resolves_namespaced_name() {
        let p = params(&[("id", "1")]);
        assert_eq!(reverse("__test_posts:detail", &p).unwrap(), "/posts/1",);
    }

    /// Issue #380 — different namespaces resolve to different
    /// patterns even when the local name is the same (`posts:detail`
    /// vs `users:detail`).
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

    // Pattern intentionally malformed — unclosed `{`.
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
        // The test registry has no intentional duplicates among
        // `__test_*` names. Other test files in the same binary may
        // or may not register routes; just confirm the function
        // doesn't panic and returns a Vec of names (sorted).
        let dups = duplicates();
        // Sort property: every two adjacent entries are ordered.
        for w in dups.windows(2) {
            assert!(w[0] <= w[1], "duplicates() must return sorted: {dups:?}");
        }
    }
}

// ============================================================== Tera tag

/// Register Django's `{% url %}` equivalent as a Tera function on
/// `tera`. Call this once at app setup alongside any other Tera
/// configuration. Issue #14.
///
/// Tera doesn't have Django-style `{% tag %}` syntax for arbitrary
/// function calls — it has function-call expressions `{{ func(...) }}`
/// — so the natural mapping is a `url(...)` function with keyword
/// arguments:
///
/// ```jinja
/// <a href="{{ url(name='post-detail', id=42) }}">View post</a>
/// ```
///
/// Equivalent to Django's `{% url 'post-detail' id=42 %}`. For the
/// `{% url 'foo' as my_url %}` capture pattern, use Tera's `{% set %}`:
///
/// ```jinja
/// {% set my_url = url(name='post-detail', id=42) %}
/// <a href="{{ my_url }}">…</a>
/// ```
///
/// Argument parsing:
/// - `name` (required): the registered route name, as a string.
/// - Every other keyword argument: a path parameter, stringified and
///   passed through [`reverse_owned`]. Numbers / bools are accepted
///   and rendered with their `Display` form.
///
/// Errors from [`reverse`] propagate as Tera render errors (rendered
/// as a 500 by [`crate::shortcuts::render`] / [`crate::template_views`]).
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
    // Every non-`name` arg becomes a path parameter. Coerce numbers /
    // bools / strings via their JSON Display form; reject objects + arrays
    // (path params can't reasonably be composite).
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
                // A null param is almost always a template typo
                // (`{{ url(name='post', id=missing_var) }}` where
                // `missing_var` is undefined). Silently emitting
                // `/posts/` would mask the bug; error explicitly
                // so the developer sees it.
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
        // Tera passes `42` as a number; `url()` stringifies via Display.
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
        // Django's `{% url 'foo' as bar %}` shape, ported to Tera's
        // `{% set %}`.
        let tera = setup();
        let src = "{% set u = url(name='__test_tag_post', id=7) %}<a href='{{ u }}'>x</a>";
        assert_eq!(render(&tera, src), "<a href='/posts/7'>x</a>");
    }

    /// Walk a Tera error's `source` chain into one searchable string.
    /// Tera wraps function errors so `format!("{e}")` on the outer
    /// only says "Failed to render '_'" — the actual cause is on
    /// `e.source()`.
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
        // Regression: a context value set to JSON null hits the
        // function as `Value::Null`. Earlier draft coerced that to ""
        // and emitted `/posts/` — a silent URL bug. Now it errors
        // explicitly so the typo / data hole surfaces.
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

/// Register the Django 5.1 `{% querystring %}` equivalent as a Tera
/// filter. Issue #19. Takes the current querystring as input and
/// returns a new one with the given overrides applied.
///
/// Foundational for paginator / filter-preserving links where you
/// want "the same URL but with `page=3` instead of `page=2`":
///
/// ```jinja
/// <!-- request.query_string = "q=hello&page=1&sort=asc" -->
/// <a href="?{{ request.query_string | querystring(page=2) | safe }}">page 2</a>
/// <!--                                       ↑ → "?q=hello&page=2&sort=asc" -->
/// ```
///
/// Behavior matches Django's [`{% querystring %}`](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#querystring):
///
/// - Each override either **replaces** the existing key or **appends**
///   a new one. Single value per key.
/// - **`null` value removes the key entirely.** Lets templates drop a
///   query param: `{{ qs | querystring(filter=null) }}`.
/// - Empty result emits an empty string (no leading `?`) so
///   `<a href="{{ '' | querystring() }}">` doesn't dangle a `?`.
/// - Keys and values are percent-encoded on output via
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
            // Null = delete every occurrence of this key.
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
        // Match Django's `QueryDict.__setitem__` semantics: if the key
        // already exists, replace IN PLACE (preserve position) +
        // collapse any duplicate occurrences down to one. New keys
        // append at the end.
        let mut found = false;
        let mut i = 0;
        while i < pairs.len() {
            if pairs[i].0 == *k {
                if found {
                    // Already kept the first slot for the override —
                    // drop subsequent duplicates of this key.
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

/// Parse a querystring (with or without the leading `?`) into a
/// list of `(key, value)` pairs preserving original order. Malformed
/// pairs (no `=`, multiple `=`) are passed through with empty / first-
/// `=`-split values — same loose interpretation as browsers.
///
/// Django-parity `urllib.parse.parse_qsl(qs)`. Strips a leading
/// `?` so callers can pass either the bare query string or the
/// full `?key=val&…` form.
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

/// Django-parity `urllib.parse.parse_qs(qs)` — parse a query
/// string into a `HashMap<String, Vec<String>>`. Same value as
/// [`parse_query_pairs`] but with multi-value keys collected into
/// a vec per key (the canonical "give me everything for key X"
/// shape).
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
        // Django parity: `page` keeps its position (between `q` and
        // `sort`) — the value updates in place rather than moving to
        // the end. Matches `QueryDict.__setitem__` semantics.
        assert_eq!(out, "?q=hello&page=2&sort=asc");
    }

    #[test]
    fn override_collapses_duplicate_existing_keys() {
        // Multi-value input — Django's QueryDict replaces the entire
        // value list with `[new]` so duplicates collapse to one.
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
        // Defensive — browsers sometimes emit `?q=x&` with a trailing
        // separator. Filter out empty chunks.
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
        // The classic open-redirect attack: `//evil.com/path` parses
        // as scheme-relative and bypasses naive `startswith('/')`
        // safety checks.
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
        // example.com whitelisted but the URL points elsewhere.
        assert!(!url_has_allowed_host_and_scheme(
            "https://evil.com/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_rejects_userinfo_smuggling() {
        // Classic phishing: `http://example.com@evil.com/` — naïve
        // parsers see `example.com` as the host but browsers route
        // to `evil.com`. Django strips userinfo and uses the real
        // host for the check.
        assert!(!url_has_allowed_host_and_scheme(
            "http://example.com@evil.com/x",
            &["example.com"],
            false
        ));
    }

    #[test]
    fn safe_url_strips_port_before_comparing() {
        // `example.com:8080` should match `example.com` allowlist.
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
        // CRLF / null injection — short-circuits URL parsers and
        // smuggles header lines into a Location redirect.
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
        // `mailto:foo@bar.com` has no `://` → not absolute, not relative.
        assert!(!url_has_allowed_host_and_scheme(
            "mailto:foo@bar.com",
            &[],
            false
        ));
        // `vbscript:msgbox(1)` same story.
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
        // `flag&q=hello` — `flag` is a bare key, no `=`.
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
        // The predicate is "is absolute," not "is safe" — dangerous
        // schemes still count as absolute. Caller pairs with
        // url_has_allowed_host_and_scheme for the policy check.
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
        // A `_` in the scheme position is not RFC 3986 valid.
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
        // `//evil.com` is the classic open-redirect vector — browsers
        // interpret as `https://evil.com` when scheme is missing.
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
        // Both `/\` and `\/` are recognized as path-confusion vectors
        // in some browser parsers; escape both.
        assert_eq!(escape_leading_slashes("/\\evil"), "/%5Cevil");
        assert_eq!(escape_leading_slashes("\\/evil"), "\\%2Fevil");
    }

    #[test]
    fn escape_leading_slashes_only_touches_the_first_two_chars() {
        // A `//` later in the URL is NOT escaped — only the leading
        // prefix matters for open-redirect prevention.
        assert_eq!(
            escape_leading_slashes("/path//slashes//inside"),
            "/path//slashes//inside"
        );
    }
}
