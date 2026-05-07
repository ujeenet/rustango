//! Pagination helpers — Link headers (RFC 5988) + cursor parameters.
//!
//! Pairs with the ViewSet's built-in pagination, but is also useful for
//! hand-written endpoints that want consistent pagination headers.
//!
//! ## Quick start — Link header builder
//!
//! ```ignore
//! use rustango::pagination::{LinkHeaderBuilder, PageInfo};
//!
//! let info = PageInfo { current_page: 2, total_pages: 5 };
//! let link = LinkHeaderBuilder::new("/api/posts")
//!     .with_page_info(info)
//!     .build();
//! // → "</api/posts?page=1>; rel=\"first\", </api/posts?page=1>; rel=\"prev\",
//! //    </api/posts?page=3>; rel=\"next\", </api/posts?page=5>; rel=\"last\""
//! ```

use std::collections::BTreeMap;

/// Page metadata for `LinkHeaderBuilder::with_page_info`.
#[derive(Debug, Clone, Copy)]
pub struct PageInfo {
    /// 1-based current page number.
    pub current_page: i64,
    /// Total number of pages (>= 1).
    pub total_pages: i64,
}

/// Builder for an RFC 5988 `Link` header.
pub struct LinkHeaderBuilder {
    base_url: String,
    extra_query: BTreeMap<String, String>,
    rels: Vec<(String, String)>, // (rel, page-or-cursor query value)
}

impl LinkHeaderBuilder {
    /// Start a builder for `base_url` (path with optional existing query).
    /// The base URL appears in every emitted link.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            extra_query: BTreeMap::new(),
            rels: Vec::new(),
        }
    }

    /// Preserve a query parameter across pagination links (e.g. `?search=foo`).
    /// Add filters/search/ordering values here so the next/prev links carry them.
    #[must_use]
    pub fn keep_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_query.insert(key.into(), value.into());
        self
    }

    /// Auto-populate `first`/`prev`/`next`/`last` rel links from page-number info.
    #[must_use]
    pub fn with_page_info(mut self, info: PageInfo) -> Self {
        if info.total_pages > 1 {
            self.rels.push(("first".into(), "1".into()));
            self.rels
                .push(("last".into(), info.total_pages.to_string()));
            if info.current_page > 1 {
                self.rels
                    .push(("prev".into(), (info.current_page - 1).to_string()));
            }
            if info.current_page < info.total_pages {
                self.rels
                    .push(("next".into(), (info.current_page + 1).to_string()));
            }
        }
        self
    }

    /// Add a single named rel pointing at `page`.
    #[must_use]
    pub fn rel(mut self, rel: impl Into<String>, page: i64) -> Self {
        self.rels.push((rel.into(), page.to_string()));
        self
    }

    /// Add a cursor-style rel: `?cursor=<token>`.
    #[must_use]
    pub fn cursor_rel(mut self, rel: impl Into<String>, cursor: impl Into<String>) -> Self {
        self.rels
            .push((format!("cursor:{}", rel.into()), cursor.into()));
        self
    }

    /// Build the final `Link` header value.
    #[must_use]
    pub fn build(&self) -> String {
        let mut entries: Vec<String> = Vec::new();
        for (rel, value) in &self.rels {
            let (param_key, rel_str) = if let Some(stripped) = rel.strip_prefix("cursor:") {
                ("cursor", stripped)
            } else {
                ("page", rel.as_str())
            };
            let mut url = self.base_url.clone();
            let mut query = self.extra_query.clone();
            query.insert(param_key.to_owned(), value.clone());
            url.push('?');
            let qs: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
                .collect();
            url.push_str(&qs.join("&"));
            entries.push(format!("<{url}>; rel=\"{rel_str}\""));
        }
        entries.join(", ")
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// =====================================================================
// PageLinks — JSON-friendly URL bundle for inline `_links` responses
// =====================================================================

const MAX_PAGE_SIZE: usize = 1000;

/// Standard pagination link bundle. Pairs with [`LinkHeaderBuilder`]
/// (header form) — `PageLinks` is the JSON-body form most APIs embed
/// inline under `_links` or alongside the result set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageLinks {
    pub current: Option<String>,
    pub first: Option<String>,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub last: Option<String>,
}

impl PageLinks {
    /// Render the bundle as a JSON object — embed under `_links` or
    /// similar in your list response.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for (k, v) in [
            ("current", &self.current),
            ("first", &self.first),
            ("prev", &self.prev),
            ("next", &self.next),
            ("last", &self.last),
        ] {
            if let Some(url) = v {
                m.insert(k.into(), serde_json::Value::String(url.clone()));
            }
        }
        serde_json::Value::Object(m)
    }

    /// Render the RFC 5988 `Link:` header value:
    /// `<url>; rel="next", <url>; rel="prev"`. Returns `None` when no
    /// links are populated.
    #[must_use]
    pub fn to_link_header(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (rel, url) in [
            ("first", &self.first),
            ("prev", &self.prev),
            ("next", &self.next),
            ("last", &self.last),
        ] {
            if let Some(u) = url {
                parts.push(format!(r#"<{u}>; rel="{rel}""#));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

/// Build page-number links from base URL + current page (1-based) +
/// page size + total row count.
///
/// Returns sensible nones for edges (no `prev` on page 1, no `next`
/// when on the last page, `first`/`last` omitted when count is 0).
/// Existing query parameters in `base` (filters, search, ordering)
/// are preserved on every emitted link.
#[must_use]
pub fn page_number_links(base: &str, page: usize, page_size: usize, count: usize) -> PageLinks {
    let page = page.max(1);
    let page_size = page_size.max(1).min(MAX_PAGE_SIZE);
    let last_page = count.div_ceil(page_size);
    let mut links = PageLinks {
        current: Some(page_number_url(base, page, page_size)),
        ..PageLinks::default()
    };
    if last_page > 0 {
        links.first = Some(page_number_url(base, 1, page_size));
        links.last = Some(page_number_url(base, last_page, page_size));
    }
    if page > 1 {
        links.prev = Some(page_number_url(base, page - 1, page_size));
    }
    if page < last_page {
        links.next = Some(page_number_url(base, page + 1, page_size));
    }
    links
}

/// Cursor links — only `next` / `current` / `first` are meaningful
/// since cursor pagination doesn't carry a total count.
#[must_use]
pub fn cursor_links(
    base: &str,
    current_cursor: Option<&str>,
    next_cursor: Option<&str>,
    page_size: usize,
) -> PageLinks {
    let page_size = page_size.max(1).min(MAX_PAGE_SIZE);
    PageLinks {
        current: current_cursor.map(|c| cursor_url(base, Some(c), page_size)),
        first: Some(cursor_url(base, None, page_size)),
        prev: None,
        next: next_cursor.map(|c| cursor_url(base, Some(c), page_size)),
        last: None,
    }
}

fn page_number_url(base: &str, page: usize, page_size: usize) -> String {
    let mut params = base_params(base);
    params.insert("page".into(), page.to_string());
    params.insert("page_size".into(), page_size.to_string());
    join_url(strip_query(base), &params)
}

fn cursor_url(base: &str, cursor: Option<&str>, page_size: usize) -> String {
    let mut params = base_params(base);
    if let Some(c) = cursor {
        params.insert("cursor".into(), c.to_owned());
    } else {
        params.remove("cursor");
    }
    params.insert("page_size".into(), page_size.to_string());
    join_url(strip_query(base), &params)
}

fn base_params(base: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(qs) = base.split('?').nth(1) {
        for pair in qs.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            // Drop the params we'll override below.
            if !matches!(k, "page" | "page_size" | "cursor") {
                out.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    out
}

fn strip_query(base: &str) -> &str {
    base.split_once('?').map_or(base, |(b, _)| b)
}

fn join_url(path: &str, params: &BTreeMap<String, String>) -> String {
    if params.is_empty() {
        return path.to_owned();
    }
    let qs = params
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                k.clone()
            } else {
                format!("{}={}", url_encode(k), url_encode(v))
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{qs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_info_middle_emits_all_four_rels() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo {
                current_page: 3,
                total_pages: 5,
            })
            .build();
        assert!(h.contains(r#"rel="first""#));
        assert!(h.contains(r#"rel="prev""#));
        assert!(h.contains(r#"rel="next""#));
        assert!(h.contains(r#"rel="last""#));
        assert!(h.contains("page=2")); // prev
        assert!(h.contains("page=4")); // next
    }

    #[test]
    fn page_info_first_page_omits_prev() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo {
                current_page: 1,
                total_pages: 5,
            })
            .build();
        assert!(!h.contains(r#"rel="prev""#));
        assert!(h.contains(r#"rel="next""#));
    }

    #[test]
    fn page_info_last_page_omits_next() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo {
                current_page: 5,
                total_pages: 5,
            })
            .build();
        assert!(h.contains(r#"rel="prev""#));
        assert!(!h.contains(r#"rel="next""#));
    }

    #[test]
    fn page_info_single_page_emits_nothing() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo {
                current_page: 1,
                total_pages: 1,
            })
            .build();
        assert_eq!(h, "");
    }

    #[test]
    fn keep_param_preserves_filter() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .keep_param("search", "rust")
            .with_page_info(PageInfo {
                current_page: 1,
                total_pages: 3,
            })
            .build();
        assert!(h.contains("search=rust"));
        assert!(h.contains("page=2"));
    }

    #[test]
    fn keep_param_url_encodes_values() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .keep_param("q", "hello world & friends")
            .rel("next", 2)
            .build();
        assert!(h.contains("q=hello%20world%20%26%20friends"));
    }

    #[test]
    fn cursor_rel_uses_cursor_param() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .cursor_rel("next", "MTIzNDU")
            .build();
        assert!(h.contains("cursor=MTIzNDU"));
        assert!(h.contains(r#"rel="next""#));
    }

    #[test]
    fn manual_rel_emits_page_param() {
        let h = LinkHeaderBuilder::new("/api/posts").rel("self", 3).build();
        assert!(h.contains("page=3"));
        assert!(h.contains(r#"rel="self""#));
    }

    #[test]
    fn multiple_entries_comma_separated() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo {
                current_page: 2,
                total_pages: 5,
            })
            .build();
        // Should be a single comma-separated string
        let count = h.matches("rel=").count();
        assert_eq!(count, 4);
        assert!(h.contains(", "));
    }

    // -------- PageLinks helpers

    #[test]
    fn first_page_no_prev() {
        let l = page_number_links("/posts", 1, 20, 200);
        assert!(l.prev.is_none());
        assert_eq!(l.next.as_deref(), Some("/posts?page=2&page_size=20"));
        assert_eq!(l.first.as_deref(), Some("/posts?page=1&page_size=20"));
        assert_eq!(l.last.as_deref(), Some("/posts?page=10&page_size=20"));
    }

    #[test]
    fn last_page_no_next() {
        let l = page_number_links("/posts", 10, 20, 200);
        assert_eq!(l.prev.as_deref(), Some("/posts?page=9&page_size=20"));
        assert!(l.next.is_none());
    }

    #[test]
    fn middle_page_has_all_links() {
        let l = page_number_links("/posts", 5, 20, 200);
        assert!(l.first.is_some());
        assert!(l.prev.is_some());
        assert!(l.next.is_some());
        assert!(l.last.is_some());
        assert_eq!(l.current.as_deref(), Some("/posts?page=5&page_size=20"));
    }

    #[test]
    fn empty_count_omits_first_and_last() {
        let l = page_number_links("/posts", 1, 20, 0);
        assert!(l.first.is_none());
        assert!(l.last.is_none());
        assert!(l.next.is_none());
        assert!(l.prev.is_none());
    }

    #[test]
    fn last_page_calculated_with_div_ceil() {
        let l = page_number_links("/posts", 1, 20, 201);
        assert_eq!(l.last.as_deref(), Some("/posts?page=11&page_size=20"));
    }

    #[test]
    fn page_size_clamped_to_max_1000() {
        let l = page_number_links("/posts", 1, 999_999, 100);
        assert!(l.current.unwrap().contains("page_size=1000"));
    }

    #[test]
    fn page_zero_treated_as_page_one() {
        let l = page_number_links("/posts", 0, 20, 200);
        assert_eq!(l.current.as_deref(), Some("/posts?page=1&page_size=20"));
    }

    #[test]
    fn existing_filter_params_are_preserved() {
        let l = page_number_links("/posts?author_id=7&search=rust", 2, 10, 50);
        let cur = l.current.unwrap();
        assert!(cur.contains("author_id=7"));
        assert!(cur.contains("search=rust"));
        assert!(cur.contains("page=2"));
        assert!(cur.contains("page_size=10"));
    }

    #[test]
    fn existing_pagination_params_get_overridden() {
        let l = page_number_links("/posts?page=99&page_size=5&author_id=7", 2, 20, 100);
        let cur = l.current.unwrap();
        assert!(cur.contains("page=2"));
        assert!(cur.contains("page_size=20"));
        assert!(!cur.contains("page=99"));
        assert!(!cur.contains("page_size=5"));
        assert!(cur.contains("author_id=7"));
    }

    #[test]
    fn cursor_with_next_token() {
        let l = cursor_links("/posts", Some("c1"), Some("c2"), 20);
        assert_eq!(l.current.as_deref(), Some("/posts?cursor=c1&page_size=20"));
        assert_eq!(l.next.as_deref(), Some("/posts?cursor=c2&page_size=20"));
        assert_eq!(l.first.as_deref(), Some("/posts?page_size=20"));
        assert!(l.prev.is_none());
        assert!(l.last.is_none());
    }

    #[test]
    fn cursor_at_end_no_next() {
        let l = cursor_links("/posts", Some("c1"), None, 20);
        assert!(l.next.is_none());
    }

    #[test]
    fn cursor_initial_request_no_current() {
        let l = cursor_links("/posts", None, Some("c1"), 20);
        assert!(l.current.is_none());
        assert_eq!(l.next.as_deref(), Some("/posts?cursor=c1&page_size=20"));
    }

    #[test]
    fn cursor_url_encodes_special_chars() {
        let l = cursor_links("/posts", None, Some("a+b=c/d"), 20);
        let next = l.next.unwrap();
        assert!(next.contains("cursor=a%2Bb%3Dc%2Fd"));
    }

    #[test]
    fn page_links_to_value_omits_none_keys() {
        let l = PageLinks {
            current: Some("/x".into()),
            first: Some("/x?page=1".into()),
            ..PageLinks::default()
        };
        let v = l.to_value();
        assert_eq!(v["current"], "/x");
        assert_eq!(v["first"], "/x?page=1");
        assert!(v.get("next").is_none());
    }

    #[test]
    fn page_links_to_link_header_renders_rfc5988_form() {
        let l = page_number_links("/posts", 5, 20, 200);
        let h = l.to_link_header().unwrap();
        assert!(h.contains(r#"; rel="next""#));
        assert!(h.contains(r#"; rel="prev""#));
        assert!(h.contains(r#"; rel="first""#));
        assert!(h.contains(r#"; rel="last""#));
        assert!(h.contains(", "));
    }

    #[test]
    fn page_links_to_link_header_returns_none_when_empty() {
        let l = PageLinks::default();
        assert!(l.to_link_header().is_none());
    }
}
