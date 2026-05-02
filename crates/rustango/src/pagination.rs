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
            self.rels.push(("last".into(), info.total_pages.to_string()));
            if info.current_page > 1 {
                self.rels.push(("prev".into(), (info.current_page - 1).to_string()));
            }
            if info.current_page < info.total_pages {
                self.rels.push(("next".into(), (info.current_page + 1).to_string()));
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
        self.rels.push((format!("cursor:{}", rel.into()), cursor.into()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_info_middle_emits_all_four_rels() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo { current_page: 3, total_pages: 5 })
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
            .with_page_info(PageInfo { current_page: 1, total_pages: 5 })
            .build();
        assert!(!h.contains(r#"rel="prev""#));
        assert!(h.contains(r#"rel="next""#));
    }

    #[test]
    fn page_info_last_page_omits_next() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo { current_page: 5, total_pages: 5 })
            .build();
        assert!(h.contains(r#"rel="prev""#));
        assert!(!h.contains(r#"rel="next""#));
    }

    #[test]
    fn page_info_single_page_emits_nothing() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .with_page_info(PageInfo { current_page: 1, total_pages: 1 })
            .build();
        assert_eq!(h, "");
    }

    #[test]
    fn keep_param_preserves_filter() {
        let h = LinkHeaderBuilder::new("/api/posts")
            .keep_param("search", "rust")
            .with_page_info(PageInfo { current_page: 1, total_pages: 3 })
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
            .with_page_info(PageInfo { current_page: 2, total_pages: 5 })
            .build();
        // Should be a single comma-separated string
        let count = h.matches("rel=").count();
        assert_eq!(count, 4);
        assert!(h.contains(", "));
    }
}
