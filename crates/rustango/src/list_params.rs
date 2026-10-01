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
//! * [`parse_page`] / [`page_offset`] — `?page=N` and its overflow-safe offset.
//! * [`split_in_list`] — a `__in` value, capped at [`MAX_IN_VALUES`];
//!   [`in_values_budget`] caps all lists of a request together.
//!
//! Each layer still builds its own WHERE clause, because viewset
//! supports a richer filter syntax than the CBV.
//!
//! [`RESERVED_LIST_KEYS`]: crate::list_params::RESERVED_LIST_KEYS
//! [`is_reserved_list_key`]: crate::list_params::is_reserved_list_key
//! [`parse_ordering`]: crate::list_params::parse_ordering
//! [`clamp_page_size`]: crate::list_params::clamp_page_size
//! [`parse_page`]: crate::list_params::parse_page
//! [`page_offset`]: crate::list_params::page_offset
//! [`split_in_list`]: crate::list_params::split_in_list
//! [`MAX_IN_VALUES`]: crate::list_params::MAX_IN_VALUES
//! [`in_values_budget`]: crate::list_params::in_values_budget

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

/// Most values one `?field__in=` list may carry. Far under every
/// dialect's bind limit, so a long list is a 400, not a driver error (#1865).
pub const MAX_IN_VALUES: usize = 1000;

/// Binds a list request keeps free for its other filters, search and paging.
const RESERVED_BINDS: usize = 1000;

/// A `__in` list longer than [`MAX_IN_VALUES`], or `__in` lists whose sum
/// passes [`in_values_budget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InListTooLong {
    /// One list is over [`MAX_IN_VALUES`].
    One,
    /// All lists together are over this budget.
    Total(usize),
}

impl std::fmt::Display for InListTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::One => write!(f, "an `__in` filter takes at most {MAX_IN_VALUES} values"),
            Self::Total(n) => write!(f, "the `__in` filters take at most {n} values in total"),
        }
    }
}

/// Most `__in` values one request may carry over all its lists, so a dialect
/// with `max_bind_params` binds gets a 400 rather than a driver 500.
#[must_use]
pub fn in_values_budget(max_bind_params: usize) -> usize {
    max_bind_params.saturating_sub(RESERVED_BINDS)
}

/// Split a comma-separated `__in` value, dropping empty entries.
/// Stops reading past the cap, so a 2 MB body is not split in full.
///
/// # Errors
/// [`InListTooLong`] when more than [`MAX_IN_VALUES`] entries remain.
pub fn split_in_list(raw: &str) -> Result<Vec<&str>, InListTooLong> {
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(MAX_IN_VALUES + 1)
        .collect();
    if parts.len() > MAX_IN_VALUES {
        return Err(InListTooLong::One);
    }
    Ok(parts)
}

/// `?page=N`, 1-based. Missing, garbage, zero or negative gives page 1.
#[must_use]
pub fn parse_page(params: &HashMap<String, String>) -> i64 {
    params
        .get("page")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1)
}

/// Row offset of 1-based `page`. Saturates, so a huge page is an
/// empty page rather than a wrapped negative OFFSET (#1865).
#[must_use]
pub fn page_offset(page: i64, page_size: i64) -> i64 {
    page.max(1)
        .saturating_sub(1)
        .saturating_mul(page_size.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_offset_saturates_instead_of_wrapping() {
        assert_eq!(page_offset(1, 20), 0);
        assert_eq!(page_offset(3, 20), 40);
        assert_eq!(page_offset(i64::MAX, 50), i64::MAX);
        assert_eq!(page_offset(i64::MIN, 50), 0);
    }

    #[test]
    fn parse_page_clamps_to_one() {
        let mut p = HashMap::new();
        assert_eq!(parse_page(&p), 1);
        p.insert("page".into(), "-4".into());
        assert_eq!(parse_page(&p), 1);
        p.insert("page".into(), i64::MAX.to_string());
        assert_eq!(parse_page(&p), i64::MAX);
    }

    #[test]
    fn in_list_cap_is_under_every_dialect_bind_limit() {
        use crate::sql::Dialect as _;
        assert!(MAX_IN_VALUES < crate::sql::Postgres.max_bind_params());
        assert!(MAX_IN_VALUES < crate::sql::MySql.max_bind_params());
        assert!(MAX_IN_VALUES < crate::sql::Sqlite.max_bind_params());
    }

    #[test]
    fn split_in_list_caps_values() {
        let ok = vec!["1"; MAX_IN_VALUES].join(",");
        assert_eq!(split_in_list(&ok).unwrap().len(), MAX_IN_VALUES);
        let long = vec!["1"; MAX_IN_VALUES + 1].join(",");
        assert_eq!(split_in_list(&long), Err(InListTooLong::One));
        assert_eq!(split_in_list("1,,2, ").unwrap(), ["1", "2"]);
    }

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
