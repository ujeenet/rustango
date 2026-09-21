//! Query-parameter parsing shared by list endpoints.
//!
//! [`viewset::handle_list`](crate::viewset) and the
//! [`ListView`](crate::template_views::ListView) CBV read the same
//! parameters: `?page=N`, `?ordering=col`, `?search=term`, and the
//! model's own filter keys. This module holds the parts they must
//! agree on:
//!
//! * [`RESERVED_LIST_KEYS`] / [`is_reserved_list_key`] — parameters
//!   that control paging, sorting and search, so they are never
//!   filters.
//! * [`parse_ordering`] — splits `?ordering=col,-col2`.
//! * [`clamp_page_size`] — resolves `?page_size=N`.
//!
//! Each layer still builds its own WHERE clause, because viewset
//! supports a richer filter syntax than the CBV.
//!
//! [`RESERVED_LIST_KEYS`]: crate::list_params::RESERVED_LIST_KEYS
//! [`is_reserved_list_key`]: crate::list_params::is_reserved_list_key
//! [`parse_ordering`]: crate::list_params::parse_ordering
//! [`clamp_page_size`]: crate::list_params::clamp_page_size

use std::collections::HashMap;

use crate::core::{ModelSchema, OrderItem};

/// Query keys that control paging, sorting and search. A list
/// handler must never treat one of these as a model filter, even if
/// a column has the same name.
pub const RESERVED_LIST_KEYS: &[&str] = &[
    "page",
    "page_size",
    "ordering",
    "search",
    "cursor",
    "limit",
    "offset",
];

/// `true` when `key` is one of [`RESERVED_LIST_KEYS`]. Use it to
/// skip these keys before parsing filters:
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

/// Parse `?ordering=col,-col2,col3` into [`OrderItem`]s. A leading
/// `-` means descending.
///
/// A token is dropped, without error, when it is not in `allowlist`
/// or when [`ModelSchema::field`] does not know it. Keep the
/// allowlist non-empty so a client cannot sort by a column such as
/// `password_hash`; an empty allowlist permits every known field.
///
/// The result is empty when nothing survives, and the caller should
/// then fall back to its default ordering.
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

/// Work out the page size from `?page_size=N`.
///
/// A value above `max` is clamped to `max`, which stops a client
/// asking for a million rows. A missing, unparseable, zero or
/// negative value gives `default`.
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
        for k in [
            "page",
            "page_size",
            "ordering",
            "search",
            "cursor",
            "limit",
            "offset",
        ] {
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
