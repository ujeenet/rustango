//! Shared helpers for list-endpoint query-parameter parsing.
//!
//! [`viewset::handle_list`](crate::viewset) and the
//! [`ListView`](crate::template_views::ListView) CBV both consume the
//! same URL-encoded query parameters (`?page=N`, `?ordering=col`,
//! `?search=term`, plus per-model filter keys). Before #809 they each
//! reimplemented the reserved-key skip list and the `-`-prefix
//! ordering split independently, and the reserved lists had drifted:
//! viewset skipped `cursor` and `ordering`; the ListView CBV did not,
//! so a column literally named `ordering` would be silently treated
//! as a filter by one layer and skipped by the other.
//!
//! This module exposes the single source of truth:
//!
//! * [`RESERVED_LIST_KEYS`] / [`is_reserved_list_key`] — the param
//!   names that are NEVER candidate filters (pagination + sort +
//!   search controls).
//! * [`parse_ordering`] — the `?ordering=col,-col2,col3` split with
//!   the `-`-prefix DESC marker, allowlist filter, and schema-field
//!   lookup.
//! * [`clamp_page_size`] — the `?page_size=N` resolver with the
//!   per-endpoint default + max-cap.
//!
//! Only the safe subset is centralized. The WHERE-clause + search
//! builders in viewset (Django-shape `__lookup` suffixes, typed
//! SqlValue via `parse_form_value`, IR `SearchClause`) and
//! template_views (exact-match Eq + ILIKE OR-folded into WHERE) are
//! intentionally divergent — merging them would regress viewset's
//! richer filter surface.
//!
//! See [issue #809](https://github.com/ujeenet/rustango/issues/809).

use std::collections::HashMap;

use crate::core::{ModelSchema, OrderItem};

/// Reserved query-string keys that NEVER match a model field — they
/// drive pagination, sort, search, and cursor flow. List handlers
/// must skip these when interpreting params as filter clauses.
pub const RESERVED_LIST_KEYS: &[&str] = &["page", "page_size", "ordering", "search", "cursor"];

/// `true` when `key` is one of [`RESERVED_LIST_KEYS`]. Use to gate
/// filter parsing:
///
/// ```ignore
/// for (k, v) in &params {
///     if rustango::list_params::is_reserved_list_key(k) {
///         continue;
///     }
///     // ... treat as filter ...
/// }
/// ```
#[must_use]
pub fn is_reserved_list_key(key: &str) -> bool {
    RESERVED_LIST_KEYS.iter().any(|r| *r == key)
}

/// Parse the `?ordering=col,-col2,col3` URL parameter into a list of
/// [`OrderItem`]s.
///
/// Each comma-separated token may carry a leading `-` to flip the
/// sort to DESC (Django / DRF convention). Tokens are filtered:
///
/// * If `allowlist` is non-empty, only field names present in it are
///   honored. Off-allowlist tokens are silently dropped (matches
///   DRF's defensive default — a hostile client can't sort on
///   `password_hash` just because it's a column).
/// * The field name (post-`-`-strip) must resolve via
///   [`ModelSchema::field`]. Unknown fields are silently dropped.
///
/// Returns an empty `Vec` when:
/// * `raw` is empty or all-whitespace
/// * every token is filtered out
///
/// Callers can detect "no override applied" by checking the result
/// against the raw input — or by passing `raw=None` to skip the
/// override and use the endpoint's default ordering.
#[must_use]
pub fn parse_ordering(
    raw: &str,
    allowlist: &[String],
    schema: &'static ModelSchema,
) -> Vec<OrderItem> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|part| {
            let (field_name, desc) = if let Some(name) = part.strip_prefix('-') {
                (name, true)
            } else {
                (part, false)
            };
            if !allowlist.is_empty() && !allowlist.iter().any(|f| f == field_name) {
                return None;
            }
            schema
                .field(field_name)
                .map(|f| OrderItem::column(f.column, desc))
        })
        .collect()
}

/// Resolve the effective `page_size` from the URL: pick the
/// `?page_size=N` value when valid + below the cap, else fall back
/// to `default`.
///
/// `default` is the endpoint's preferred page size (typically 20–50).
/// `max` is the hard cap that protects the backend from runaway
/// requests — passing `?page_size=999999` clamps to `max`.
///
/// Edge cases handled:
/// * Missing / empty / non-parseable param → `default`.
/// * Negative or zero → `default` (a 0-row "page" doesn't make sense).
/// * Larger than `max` → clamped to `max`.
#[must_use]
pub fn clamp_page_size(default: i64, max: i64, params: &HashMap<String, String>) -> i64 {
    params
        .get("page_size")
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map_or(default, |n| n.min(max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_keys_match_documented_set() {
        for k in ["page", "page_size", "ordering", "search", "cursor"] {
            assert!(is_reserved_list_key(k), "{k} should be reserved");
        }
        assert!(!is_reserved_list_key("name"));
        assert!(!is_reserved_list_key("created_at"));
        assert!(!is_reserved_list_key(""));
    }

    #[test]
    fn clamp_page_size_picks_param_when_valid() {
        let mut p = HashMap::new();
        p.insert("page_size".into(), "30".into());
        assert_eq!(clamp_page_size(20, 100, &p), 30);
    }

    #[test]
    fn clamp_page_size_caps_at_max() {
        let mut p = HashMap::new();
        p.insert("page_size".into(), "999".into());
        assert_eq!(clamp_page_size(20, 100, &p), 100);
    }

    #[test]
    fn clamp_page_size_falls_back_on_missing() {
        let p = HashMap::new();
        assert_eq!(clamp_page_size(20, 100, &p), 20);
    }

    #[test]
    fn clamp_page_size_falls_back_on_garbage() {
        let mut p = HashMap::new();
        p.insert("page_size".into(), "not-a-number".into());
        assert_eq!(clamp_page_size(20, 100, &p), 20);
    }

    #[test]
    fn clamp_page_size_falls_back_on_zero_or_negative() {
        let mut p = HashMap::new();
        p.insert("page_size".into(), "0".into());
        assert_eq!(clamp_page_size(20, 100, &p), 20);
        p.insert("page_size".into(), "-5".into());
        assert_eq!(clamp_page_size(20, 100, &p), 20);
    }
}
