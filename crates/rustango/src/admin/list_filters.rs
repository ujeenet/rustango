//! Django-shape `admin.SimpleListFilter` — operator-defined facet
//! filters with custom lookup values + predicate logic. Issue #351.
//!
//! Where `list_filter = "field"` builds a facet card from the
//! distinct values of one column, a `SimpleListFilter` defines its
//! *own* finite list of lookup choices and decides what each choice
//! means as a `WhereExpr`. Examples: "decade born" (1980s / 1990s)
//! over a `birthday` column, or "active" / "stale" over a
//! `last_login_at` column.
//!
//! ## Example
//!
//! ```ignore
//! use rustango::core::{Filter, Op, SqlValue};
//!
//! fn status_to_filters(value: &str) -> Vec<Filter> {
//!     match value {
//!         "draft" => vec![Filter {
//!             column: "status",
//!             op: Op::Eq,
//!             value: SqlValue::String("draft".into()),
//!         }],
//!         "published" => vec![Filter {
//!             column: "status",
//!             op: Op::Eq,
//!             value: SqlValue::String("published".into()),
//!         }],
//!         _ => Vec::new(),
//!     }
//! }
//!
//! rustango::register_admin_list_filter!(
//!     "blog_post",
//!     "status",
//!     "Status",
//!     &[("draft", "Drafts"), ("published", "Published")],
//!     status_to_filters,
//! );
//! ```
//!
//! Visiting `/blog_post?status=draft` then applies the predicates the
//! function returns and shows the choice as selected in the filter
//! sidebar.

use crate::core::Filter;

/// Function signature a custom list filter implements. Receives the
/// active value from the URL (URL-decoded) and returns the predicates
/// to AND onto the list view's WHERE. An empty `Vec` means "no
/// narrowing" — Django's `if self.value() is None` shape.
pub type AdminListFilterFn = fn(value: &str) -> Vec<Filter>;

/// One registration. Inventory-collected; submit via the
/// [`register_admin_list_filter!`](crate::register_admin_list_filter)
/// macro.
pub struct AdminListFilter {
    /// SQL table the filter attaches to — must match
    /// `ModelSchema::table` exactly.
    pub table: &'static str,
    /// URL query parameter name the filter reads from
    /// (e.g. `"status"` for `?status=draft`).
    pub parameter_name: &'static str,
    /// Display label shown above the filter card.
    pub title: &'static str,
    /// Choices the operator sees as clickable links. Each pair is
    /// `(value, display_label)` — `value` round-trips through the URL.
    pub lookups: &'static [(&'static str, &'static str)],
    /// Predicate-emitter. Pure — receives the URL value and returns
    /// the filters to AND onto the list view's WHERE.
    pub to_filters: AdminListFilterFn,
}

inventory::collect!(AdminListFilter);

/// Yield every registered filter for `table`. Cheap — the iterator is
/// `O(N)` over the entire admin-filter registry but `N` is small
/// (bounded by the number of `register_admin_list_filter!` calls
/// across the whole binary).
pub fn for_table(table: &str) -> impl Iterator<Item = &'static AdminListFilter> + use<'_> {
    inventory::iter::<AdminListFilter>
        .into_iter()
        .filter(move |f| f.table == table)
}

/// Register a custom list filter. Pair with the table whose admin
/// list view should expose the filter card.
///
/// ```ignore
/// use rustango::core::{Filter, Op, SqlValue};
///
/// fn status_filters(v: &str) -> Vec<Filter> { /* … */ vec![] }
///
/// rustango::register_admin_list_filter!(
///     "blog_post",
///     "status",
///     "Status",
///     &[("draft", "Drafts"), ("published", "Published")],
///     status_filters,
/// );
/// ```
#[macro_export]
macro_rules! register_admin_list_filter {
    ($table:expr, $parameter_name:expr, $title:expr, $lookups:expr, $to_filters:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::admin::list_filters::AdminListFilter {
                table: $table,
                parameter_name: $parameter_name,
                title: $title,
                lookups: $lookups,
                to_filters: $to_filters,
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_compiles_with_zero_entries() {
        // No `register_admin_list_filter!` in this test binary → empty
        // iter. The point is the inventory link doesn't panic when
        // nothing's submitted.
        assert_eq!(for_table("nonexistent").count(), 0);
    }
}
