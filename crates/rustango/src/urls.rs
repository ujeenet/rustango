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
//! Namespaced reverse (Django's `reverse("app:detail")` /
//! `include("app.urls", namespace=...)`) is **out of scope for v1** —
//! the same plain name appears in the global registry. Two PRs in the
//! same binary registering `"detail"` will collide; defer namespace
//! support until users hit it.

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
    let mut used: HashSet<&str> = HashSet::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        // Read until the matching '}'. Bail with a clear error if the
        // pattern has an unclosed placeholder — that's a programmer
        // bug, but the message is more useful than a stuck loop.
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
            return Err(ReverseError::MissingParam {
                name: name.to_owned(),
                param: format!("(unclosed `{{{placeholder}`)"),
            });
        }
        // Strip axum-style type annotations like `{id:int}` — axum 0.8
        // doesn't use them by default but Django patterns sometimes
        // carry `<int:id>`. We accept either `name` or `type:name`.
        let key = placeholder.split(':').next_back().unwrap_or(&placeholder);
        let value = params.get(key).ok_or_else(|| ReverseError::MissingParam {
            name: name.to_owned(),
            param: key.to_owned(),
        })?;
        out.push_str(&crate::url_codec::url_encode(value));
        // Find the params key with the same identity (str pointer is
        // unreliable since they come from the caller's HashMap). Look
        // it up by string equality to mark "used."
        if let Some((k, _)) = params.iter().find(|(k, _)| **k == key) {
            used.insert(*k);
        }
    }
    // Reject extra params.
    for k in params.keys() {
        if !used.contains(k) {
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
}
