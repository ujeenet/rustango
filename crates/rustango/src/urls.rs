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
//! Namespaced reverse (Django's `reverse("app:detail")` /
//! `include("app.urls", namespace=...)`) is **out of scope for v1**.
//! Use unique per-app name prefixes (`"posts:detail"` / `"users:detail"`)
//! as a manual convention until namespace support lands.

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
