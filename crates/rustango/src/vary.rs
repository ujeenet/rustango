//! `Vary` header merging shared by the response layers.

use axum::http::header::VARY;
use axum::http::{HeaderMap, HeaderValue};

/// Add `token` to `Vary` unless it (or `*`) is already listed, folding
/// every existing `Vary` value into one header.
pub(crate) fn add_vary(headers: &mut HeaderMap, token: &str) {
    let existing: Vec<&str> = headers
        .get_all(VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if existing
        .iter()
        .any(|t| *t == "*" || t.eq_ignore_ascii_case(token))
    {
        return;
    }
    let mut joined = existing.join(", ");
    if !joined.is_empty() {
        joined.push_str(", ");
    }
    joined.push_str(token);
    if let Ok(v) = HeaderValue::from_str(&joined) {
        headers.insert(VARY, v);
    }
}

/// Make `Cache-Control` `private`: any `public` is dropped, freshness
/// directives such as `max-age` are kept.
#[cfg(any(feature = "cache-page", feature = "template_views"))]
pub(crate) fn mark_private(headers: &mut HeaderMap) {
    use axum::http::header::CACHE_CONTROL;
    // Every line, so a `no-store` on a second one survives the rewrite.
    let mut directives: Vec<String> = headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|d| d.trim().to_owned())
        .filter(|d| {
            !d.is_empty() && !d.eq_ignore_ascii_case("public") && !d.eq_ignore_ascii_case("private")
        })
        .collect();
    directives.insert(0, "private".to_owned());
    if let Ok(v) = HeaderValue::from_str(&directives.join(", ")) {
        headers.insert(CACHE_CONTROL, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_and_dedupes() {
        let mut h = HeaderMap::new();
        h.append(VARY, HeaderValue::from_static("Origin"));
        h.append(VARY, HeaderValue::from_static("cookie"));
        add_vary(&mut h, "Accept-Language");
        add_vary(&mut h, "Cookie");
        assert_eq!(h.get_all(VARY).iter().count(), 1);
        assert_eq!(h[VARY], "Origin, cookie, Accept-Language");
    }

    /// Every `Cache-Control` line is kept; only `public` goes.
    #[cfg(any(feature = "cache-page", feature = "template_views"))]
    #[test]
    fn mark_private_keeps_directives_from_every_line() {
        use axum::http::header::CACHE_CONTROL;
        let mut h = HeaderMap::new();
        h.append(CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
        h.append(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        mark_private(&mut h);
        assert_eq!(h.get_all(CACHE_CONTROL).iter().count(), 1);
        assert_eq!(h[CACHE_CONTROL], "private, max-age=60, no-store");
    }

    /// `Vary: *` already covers every header, so it stays as is.
    #[test]
    fn star_is_left_alone() {
        let mut h = HeaderMap::new();
        h.insert(VARY, HeaderValue::from_static("*"));
        add_vary(&mut h, "Accept-Language");
        assert_eq!(h[VARY], "*");
    }
}
