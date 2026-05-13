//! Pagination helpers — two shapes for different surfaces.
//!
//! ## API-layer shape: `Link` headers + cursor parameters
//!
//! Pairs with the ViewSet's built-in pagination, but is also useful for
//! hand-written endpoints that want consistent pagination headers.
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
//!
//! ## Django-shape: [`Paginator`] + [`Page`]
//!
//! For server-side rendered list views (Tera / template_views), pure-
//! metadata Paginator + Page types that mirror Django's
//! `django.core.paginator.Paginator` API. The Page holds no rows — the
//! caller computes `page.offset()` and `page.limit()` and feeds them
//! into a `QuerySet` `.offset(...).limit(...).fetch_pool(...)` call.
//!
//! ```ignore
//! use rustango::pagination::Paginator;
//!
//! let total = Post::objects().count_pool(&pool).await?;
//! let paginator = Paginator::new(total as usize, 20);
//! let page = paginator.get_page(requested_page_number); // never errors — clamps
//! let rows = Post::objects()
//!     .order_by(&[("id", false)])
//!     .limit(page.limit() as i64)
//!     .offset(page.offset() as i64)
//!     .fetch_pool(&pool).await?;
//!
//! // Template rendering — emit the 1, 2, …, 12, 13, 14, …, 49, 50 pager:
//! for mark in paginator.get_elided_page_range(page.number, 3, 2) { … }
//! ```
//!
//! Mirrors Django's algorithm bit-for-bit including the
//! `(on_each_side + on_ends) * 2` short-circuit for small page counts.

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

// ============================================================================
// Django-shape Paginator + Page (issue #12)
//
// Pure-metadata types for server-side rendered list views. Mirrors
// `django.core.paginator.Paginator` / `Page` so apps porting from
// Django keep their muscle memory.
//
// Distinct from the `LinkHeaderBuilder` / `PageLinks` API-layer shape
// above. Pick the right tool: API endpoints emit RFC 5988 `Link`
// headers + cursor params; HTML list views render `<nav>` with the
// `Paginator` / `Page` types.
// ============================================================================

/// One element of [`Paginator::get_elided_page_range`] — either a page
/// number or an ellipsis marker. Templates render the marker as
/// "…" (or whatever skipped-pages indicator the design calls for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageMark {
    Number(usize),
    Ellipsis,
}

/// Errors from [`Paginator::page`] / [`Page::next_page_number`] /
/// [`Page::previous_page_number`]. Mirrors Django's
/// `EmptyPage` / `PageNotAnInteger` / `InvalidPage` exception family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PaginatorError {
    #[error("page number must be a positive integer (got 0)")]
    PageNotAnInteger,
    #[error("page {0} is empty (count = 0 and allow_empty_first_page is disabled)")]
    EmptyPage(usize),
    #[error("page {requested} is out of range; last page is {last}")]
    OutOfRange { requested: usize, last: usize },
}

/// Django-shape paginator. Pure metadata — holds no rows.
///
/// Build with [`Paginator::new`] from a `count` + `per_page`. Optional
/// builder methods narrow the behaviour:
/// - [`Paginator::orphans`] — if the last page would have <= `orphans`
///   items, roll them into the second-to-last page so the trailing
///   `<nav>` doesn't show a near-empty page.
/// - [`Paginator::allow_empty_first_page`] — when `count == 0`, return
///   page 1 as a valid empty page (default `true`, matches Django).
///
/// Call [`Paginator::page`] for explicit error handling, or
/// [`Paginator::get_page`] for the "clamp to valid range" shape.
#[derive(Debug, Clone, Copy)]
pub struct Paginator {
    /// Total number of items across all pages.
    pub count: usize,
    /// Maximum items per page. Always >= 1 (constructor clamps).
    pub per_page: usize,
    /// Items on the last page get rolled into the previous page when
    /// the count is `<= orphans`. Default 0 (Django default).
    pub orphans: usize,
    /// When `count == 0`, treat page 1 as a valid empty page. Default
    /// `true` (Django default).
    pub allow_empty_first_page: bool,
}

impl Paginator {
    /// Build a paginator. `per_page` is clamped to at least 1 (Django
    /// raises `ZeroDivisionError`; rustango opts for the clamp).
    #[must_use]
    pub fn new(count: usize, per_page: usize) -> Self {
        Self {
            count,
            per_page: per_page.max(1),
            orphans: 0,
            allow_empty_first_page: true,
        }
    }

    /// Set the orphan threshold. Items on the last page that number
    /// `<= orphans` get rolled into the previous page.
    #[must_use]
    pub fn orphans(mut self, orphans: usize) -> Self {
        self.orphans = orphans;
        self
    }

    /// Toggle the "page 1 is valid when count == 0" behaviour.
    #[must_use]
    pub fn allow_empty_first_page(mut self, allow: bool) -> Self {
        self.allow_empty_first_page = allow;
        self
    }

    /// Total number of pages. `0` when `count == 0` and empty first
    /// page is disallowed; otherwise `1` for an empty paginator.
    #[must_use]
    pub fn num_pages(&self) -> usize {
        if self.count == 0 {
            return usize::from(self.allow_empty_first_page);
        }
        let hits = self.count.saturating_sub(self.orphans).max(1);
        hits.div_ceil(self.per_page)
    }

    /// 1-based inclusive range of valid page numbers.
    #[must_use]
    pub fn page_range(&self) -> std::ops::RangeInclusive<usize> {
        1..=self.num_pages()
    }

    /// Validate a page number against the paginator. Returns the
    /// number unchanged on success. Mirrors Django's `validate_number`.
    ///
    /// # Errors
    /// - [`PaginatorError::PageNotAnInteger`] when `number < 1`.
    /// - [`PaginatorError::EmptyPage`] when `count == 0`,
    ///   `allow_empty_first_page == false`, and `number == 1`.
    /// - [`PaginatorError::OutOfRange`] when `number > num_pages()`.
    pub fn validate_number(&self, number: usize) -> Result<usize, PaginatorError> {
        if number < 1 {
            return Err(PaginatorError::PageNotAnInteger);
        }
        let last = self.num_pages();
        if last == 0 {
            // count == 0 + allow_empty_first_page == false — every
            // page is invalid.
            return Err(PaginatorError::EmptyPage(number));
        }
        if number > last {
            // Special case: count == 0, allow_empty_first_page == true
            // (so last == 1) — Django still rejects page > 1.
            return Err(PaginatorError::OutOfRange {
                requested: number,
                last,
            });
        }
        Ok(number)
    }

    /// Build a [`Page`] for `number`, validating it first.
    ///
    /// # Errors
    /// As [`Self::validate_number`].
    pub fn page(&self, number: usize) -> Result<Page<'_>, PaginatorError> {
        let valid = self.validate_number(number)?;
        Ok(Page {
            number: valid,
            paginator: self,
        })
    }

    /// Build a [`Page`] for `number`, clamping out-of-range values to
    /// page 1 (negative / zero) or the last page (too large). Never
    /// returns an error. Mirrors Django's `get_page`.
    #[must_use]
    pub fn get_page(&self, number: i64) -> Page<'_> {
        let last = self.num_pages().max(1);
        let n = if number < 1 {
            1
        } else {
            (number as usize).min(last)
        };
        Page {
            number: n,
            paginator: self,
        }
    }

    /// Yield a "1, 2, …, 12, 13, 14, …, 49, 50"-style elided page
    /// range for rendering a `<nav>` pager. Mirrors Django's
    /// `get_elided_page_range` bit-for-bit:
    /// - Short-circuits when `num_pages <= (on_each_side + on_ends) * 2`
    ///   and emits every page directly with no ellipsis.
    /// - Otherwise emits the left edge, a window around `number`, and
    ///   the right edge, with [`PageMark::Ellipsis`] markers in the
    ///   gaps.
    ///
    /// Django defaults: `on_each_side=3, on_ends=2`. An invalid
    /// `number` is clamped to page 1 (Django raises; we don't, to
    /// match `get_page`'s forgiving shape).
    #[must_use]
    pub fn get_elided_page_range(
        &self,
        number: usize,
        on_each_side: usize,
        on_ends: usize,
    ) -> Vec<PageMark> {
        let last = self.num_pages();
        if last == 0 {
            return Vec::new();
        }
        let number = self.validate_number(number).unwrap_or(1);

        // Short-circuit: small enough that every page fits in the
        // left + right windows. Emit them all without ellipsis.
        let threshold = on_each_side.saturating_add(on_ends).saturating_mul(2);
        if last <= threshold {
            return (1..=last).map(PageMark::Number).collect();
        }

        let mut out = Vec::new();

        // ---- Left half (everything up to and including `number`) ----
        // Django: if number > (1 + on_each_side + on_ends) + 1, emit
        // [1..on_ends] + ELL + [number - on_each_side .. number].
        // Else emit [1 .. number].
        let left_gap_trigger = 1usize
            .saturating_add(on_each_side)
            .saturating_add(on_ends)
            .saturating_add(1);
        if number > left_gap_trigger {
            for n in 1..=on_ends {
                out.push(PageMark::Number(n));
            }
            out.push(PageMark::Ellipsis);
            for n in number.saturating_sub(on_each_side)..=number {
                out.push(PageMark::Number(n));
            }
        } else {
            for n in 1..=number {
                out.push(PageMark::Number(n));
            }
        }

        // ---- Right half (everything after `number`) ----
        // Django: if number < (num_pages - on_each_side - on_ends) - 1,
        // emit [number+1 .. number+on_each_side] + ELL + [last-on_ends+1 .. last].
        // Else emit [number+1 .. last].
        let right_gap_trigger = last
            .saturating_sub(on_each_side)
            .saturating_sub(on_ends)
            .saturating_sub(1);
        if number < right_gap_trigger {
            for n in (number + 1)..=(number + on_each_side) {
                out.push(PageMark::Number(n));
            }
            out.push(PageMark::Ellipsis);
            for n in (last + 1 - on_ends.min(last))..=last {
                out.push(PageMark::Number(n));
            }
        } else {
            for n in (number + 1)..=last {
                out.push(PageMark::Number(n));
            }
        }

        out
    }
}

/// One page within a [`Paginator`]. Pure metadata — holds no rows.
/// Use [`Page::limit`] + [`Page::offset`] to drive a `QuerySet`
/// `.limit(...).offset(...)` chain.
#[derive(Debug, Clone, Copy)]
pub struct Page<'a> {
    /// 1-based page number.
    pub number: usize,
    /// Back-reference to the paginator (for `count` / `num_pages` /
    /// `per_page` lookups).
    pub paginator: &'a Paginator,
}

impl<'a> Page<'a> {
    /// `true` if a next page exists.
    #[must_use]
    pub fn has_next(&self) -> bool {
        self.number < self.paginator.num_pages()
    }

    /// `true` if a previous page exists.
    #[must_use]
    pub fn has_previous(&self) -> bool {
        self.number > 1
    }

    /// `true` if there's at least one other page.
    #[must_use]
    pub fn has_other_pages(&self) -> bool {
        self.has_next() || self.has_previous()
    }

    /// Next page number, or `OutOfRange` if we're on the last page.
    ///
    /// # Errors
    /// [`PaginatorError::OutOfRange`] when `!self.has_next()`.
    pub fn next_page_number(&self) -> Result<usize, PaginatorError> {
        if !self.has_next() {
            return Err(PaginatorError::OutOfRange {
                requested: self.number + 1,
                last: self.paginator.num_pages(),
            });
        }
        Ok(self.number + 1)
    }

    /// Previous page number, or `OutOfRange` if we're on page 1.
    ///
    /// # Errors
    /// [`PaginatorError::OutOfRange`] when `!self.has_previous()`.
    pub fn previous_page_number(&self) -> Result<usize, PaginatorError> {
        if !self.has_previous() {
            return Err(PaginatorError::OutOfRange {
                requested: 0,
                last: self.paginator.num_pages(),
            });
        }
        Ok(self.number - 1)
    }

    /// 1-based index of the first item on this page. `0` when
    /// `count == 0`. Useful for "Showing 41–60 of 200" UI lines.
    #[must_use]
    pub fn start_index(&self) -> usize {
        if self.paginator.count == 0 {
            return 0;
        }
        (self.number - 1) * self.paginator.per_page + 1
    }

    /// 1-based index of the last item on this page (inclusive). On
    /// the last page, equal to `count` (so the trailing-partial-page
    /// case is exact).
    #[must_use]
    pub fn end_index(&self) -> usize {
        if self.paginator.count == 0 {
            return 0;
        }
        if self.number == self.paginator.num_pages() {
            return self.paginator.count;
        }
        self.number * self.paginator.per_page
    }

    /// SQL `LIMIT` value for this page. Equals `per_page`.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.paginator.per_page
    }

    /// SQL `OFFSET` value for this page (0-based).
    #[must_use]
    pub fn offset(&self) -> usize {
        (self.number - 1) * self.paginator.per_page
    }

    /// Slice a fully-fetched item list for this page. Convenient for
    /// in-memory paging when you've already pulled everything via a
    /// single `fetch_pool` and want to render one page at a time
    /// without round-trips. For DB-backed paging, prefer
    /// `.limit([page.limit()]).offset([page.offset()])` on the queryset.
    ///
    /// [page.limit()]: Self::limit
    /// [page.offset()]: Self::offset
    #[must_use]
    pub fn slice<'b, T>(&self, items: &'b [T]) -> &'b [T] {
        let start = self.offset().min(items.len());
        let end = (start + self.limit()).min(items.len());
        &items[start..end]
    }
}

#[cfg(test)]
mod paginator_tests {
    use super::*;

    // ---------- Paginator::num_pages ----------

    #[test]
    fn num_pages_simple_math() {
        assert_eq!(Paginator::new(100, 20).num_pages(), 5);
        assert_eq!(Paginator::new(101, 20).num_pages(), 6);
        assert_eq!(Paginator::new(99, 20).num_pages(), 5);
        assert_eq!(Paginator::new(20, 20).num_pages(), 1);
        assert_eq!(Paginator::new(1, 20).num_pages(), 1);
    }

    #[test]
    fn num_pages_zero_count_respects_allow_empty_first_page() {
        assert_eq!(Paginator::new(0, 20).num_pages(), 1);
        assert_eq!(
            Paginator::new(0, 20)
                .allow_empty_first_page(false)
                .num_pages(),
            0
        );
    }

    #[test]
    fn num_pages_rolls_orphans_into_previous_page() {
        // 23 items, 20 per page, 5 orphans → last 3 items (<= 5) roll
        // into page 1, so num_pages = 1.
        let p = Paginator::new(23, 20).orphans(5);
        assert_eq!(p.num_pages(), 1);

        // 26 items, 20 per page, 5 orphans → 6 items on page 2 > 5
        // orphans, so two pages.
        let p2 = Paginator::new(26, 20).orphans(5);
        assert_eq!(p2.num_pages(), 2);
    }

    #[test]
    fn per_page_zero_clamps_to_one() {
        // No ZeroDivisionError — rustango clamps where Django raises.
        let p = Paginator::new(5, 0);
        assert_eq!(p.per_page, 1);
        assert_eq!(p.num_pages(), 5);
    }

    // ---------- Paginator::page / validate_number ----------

    #[test]
    fn page_zero_is_page_not_an_integer() {
        let p = Paginator::new(100, 20);
        assert_eq!(p.page(0).unwrap_err(), PaginatorError::PageNotAnInteger);
    }

    #[test]
    fn page_out_of_range_errors_with_last_page() {
        let p = Paginator::new(100, 20);
        let err = p.page(99).unwrap_err();
        assert_eq!(
            err,
            PaginatorError::OutOfRange {
                requested: 99,
                last: 5,
            }
        );
    }

    #[test]
    fn page_on_disallowed_empty_first_page_errors() {
        let p = Paginator::new(0, 20).allow_empty_first_page(false);
        assert_eq!(p.page(1).unwrap_err(), PaginatorError::EmptyPage(1));
    }

    #[test]
    fn page_one_on_default_empty_paginator_is_valid_empty() {
        // Django: with count=0 and allow_empty_first_page=true,
        // page(1) returns an empty page. page(2) errors.
        let p = Paginator::new(0, 20);
        let page = p.page(1).expect("page 1 valid on empty paginator");
        assert_eq!(page.number, 1);
        assert!(p.page(2).is_err());
    }

    // ---------- Paginator::get_page (clamping shape) ----------

    #[test]
    fn get_page_clamps_negative_to_first() {
        let p = Paginator::new(100, 20);
        assert_eq!(p.get_page(-5).number, 1);
        assert_eq!(p.get_page(0).number, 1);
    }

    #[test]
    fn get_page_clamps_too_large_to_last() {
        let p = Paginator::new(100, 20);
        assert_eq!(p.get_page(9_999).number, 5);
    }

    // ---------- Page::has_next / has_previous / start_index / end_index ----------

    #[test]
    fn page_navigation_flags() {
        let p = Paginator::new(50, 20); // 3 pages
        let page1 = p.page(1).unwrap();
        assert!(page1.has_next());
        assert!(!page1.has_previous());
        assert!(page1.has_other_pages());

        let page2 = p.page(2).unwrap();
        assert!(page2.has_next());
        assert!(page2.has_previous());

        let page3 = p.page(3).unwrap();
        assert!(!page3.has_next());
        assert!(page3.has_previous());
    }

    #[test]
    fn next_previous_page_number_error_on_boundaries() {
        let p = Paginator::new(50, 20);
        assert!(p.page(1).unwrap().previous_page_number().is_err());
        assert_eq!(p.page(1).unwrap().next_page_number().unwrap(), 2);
        assert!(p.page(3).unwrap().next_page_number().is_err());
        assert_eq!(p.page(3).unwrap().previous_page_number().unwrap(), 2);
    }

    #[test]
    fn start_end_index_exact_on_partial_last_page() {
        let p = Paginator::new(50, 20); // pages: 20 + 20 + 10
        let page1 = p.page(1).unwrap();
        assert_eq!(page1.start_index(), 1);
        assert_eq!(page1.end_index(), 20);

        let page3 = p.page(3).unwrap();
        assert_eq!(page3.start_index(), 41);
        assert_eq!(page3.end_index(), 50); // exact, not 60
    }

    #[test]
    fn start_end_index_zero_when_count_zero() {
        let p = Paginator::new(0, 20);
        let page = p.page(1).unwrap();
        assert_eq!(page.start_index(), 0);
        assert_eq!(page.end_index(), 0);
    }

    #[test]
    fn limit_offset_drives_sql_pagination() {
        let p = Paginator::new(100, 20);
        let page = p.page(3).unwrap();
        assert_eq!(page.limit(), 20);
        assert_eq!(page.offset(), 40); // (3-1) * 20
    }

    // ---------- Page::slice (in-memory paging) ----------

    #[test]
    fn slice_returns_correct_window() {
        let items: Vec<i32> = (1..=50).collect();
        let p = Paginator::new(items.len(), 20);
        let page2 = p.page(2).unwrap();
        let window = page2.slice(&items);
        assert_eq!(window, &(21..=40).collect::<Vec<_>>()[..]);
    }

    #[test]
    fn slice_clamps_when_data_shorter_than_announced_count() {
        // Defensive: someone passes a smaller slice than the paginator
        // was built for. Don't panic — return what's there.
        let items: Vec<i32> = (1..=10).collect();
        let p = Paginator::new(50, 20); // says 50 items, only got 10
        let page1 = p.page(1).unwrap();
        let window = page1.slice(&items);
        assert_eq!(window, &(1..=10).collect::<Vec<_>>()[..]);
        let page2 = p.page(2).unwrap();
        assert!(page2.slice(&items).is_empty());
    }

    // ---------- Paginator::get_elided_page_range — Django parity ----------

    #[test]
    fn elided_short_circuit_when_below_threshold() {
        // last = 6, on_each_side=2, on_ends=1 → threshold = (2+1)*2 = 6,
        // and 6 <= 6 so no ellipsis.
        let p = Paginator::new(120, 20); // 6 pages
        let marks = p.get_elided_page_range(3, 2, 1);
        assert_eq!(
            marks,
            (1..=6).map(PageMark::Number).collect::<Vec<_>>(),
            "small page count must short-circuit to all-pages-no-ellipsis"
        );
    }

    #[test]
    fn elided_django_canonical_example() {
        // Django docstring example: 1000 pages, current=500,
        // on_each_side=3, on_ends=2 →
        // 1, 2, …, 497, 498, 499, 500, 501, 502, 503, …, 999, 1000.
        let p = Paginator::new(20_000, 20); // 1000 pages
        let marks = p.get_elided_page_range(500, 3, 2);
        use PageMark::{Ellipsis, Number};
        assert_eq!(
            marks,
            vec![
                Number(1),
                Number(2),
                Ellipsis,
                Number(497),
                Number(498),
                Number(499),
                Number(500),
                Number(501),
                Number(502),
                Number(503),
                Ellipsis,
                Number(999),
                Number(1000),
            ]
        );
    }

    #[test]
    fn elided_near_left_edge() {
        // Current page near the start — no left ellipsis, only right.
        let p = Paginator::new(1000, 20); // 50 pages
        let marks = p.get_elided_page_range(2, 2, 1);
        use PageMark::{Ellipsis, Number};
        // Expected: 1, 2, 3, 4, …, 50  (window touches left edge)
        assert_eq!(
            marks,
            vec![
                Number(1),
                Number(2),
                Number(3),
                Number(4),
                Ellipsis,
                Number(50)
            ]
        );
    }

    #[test]
    fn elided_near_right_edge() {
        // Current page near the end — left ellipsis, no right ellipsis.
        let p = Paginator::new(1000, 20); // 50 pages
        let marks = p.get_elided_page_range(49, 2, 1);
        use PageMark::{Ellipsis, Number};
        // Expected: 1, …, 47, 48, 49, 50
        assert_eq!(
            marks,
            vec![
                Number(1),
                Ellipsis,
                Number(47),
                Number(48),
                Number(49),
                Number(50),
            ]
        );
    }

    #[test]
    fn elided_invalid_number_clamps_to_first() {
        // `get_elided_page_range` should be as forgiving as `get_page`.
        let p = Paginator::new(1000, 20);
        let marks_invalid = p.get_elided_page_range(99_999, 3, 2);
        let marks_clamped = p.get_elided_page_range(1, 3, 2);
        assert_eq!(marks_invalid, marks_clamped);
    }

    #[test]
    fn elided_empty_paginator_returns_empty_vec() {
        let p = Paginator::new(0, 20).allow_empty_first_page(false);
        assert!(p.get_elided_page_range(1, 3, 2).is_empty());
    }
}
