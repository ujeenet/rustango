//! A named filter that puts its own choices in the list sidebar and
//! decides what each choice means as a predicate.
//!
//! `list_filter = "field"` builds a facet card from the distinct
//! values of one column. A filter here instead declares a fixed list
//! of choices and decides what each one means as a predicate. For
//! example "1980s" and "1990s" over a `birthday` column.
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

/// A custom list filter. Takes the URL-decoded value from the query
/// string and returns the predicates to AND onto the list view's
/// WHERE. An empty `Vec` means no narrowing.
pub type AdminListFilterFn = fn(value: &str) -> Vec<Filter>;

/// One registration, collected by inventory. Submit it with the
/// [`register_admin_list_filter!`](crate::register_admin_list_filter)
/// macro.
pub struct AdminListFilter {
    /// SQL table the filter belongs to. Must equal
    /// `ModelSchema::table`.
    pub table: &'static str,
    /// Query parameter the filter reads, such as `"status"` for
    /// `?status=draft`.
    pub parameter_name: &'static str,
    /// Label shown above the filter card.
    pub title: &'static str,
    /// Choices rendered as links. Each pair is
    /// `(value, display_label)`, and `value` travels in the URL.
    pub lookups: &'static [(&'static str, &'static str)],
    /// Turns the URL value into the filters to AND onto the WHERE.
    pub to_filters: AdminListFilterFn,
}

inventory::collect!(AdminListFilter);

/// Every registered filter for `table`. The scan is `O(N)` over all
/// registrations in the binary, which stays small.
pub fn for_table(table: &str) -> impl Iterator<Item = &'static AdminListFilter> + use<'_> {
    inventory::iter::<AdminListFilter>
        .into_iter()
        .filter(move |f| f.table == table)
}

/// Register a custom list filter on the table whose list view should
/// show the filter card.
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
        // This test binary registers nothing, so the iterator is
        // empty. The point is that it does not panic.
        assert_eq!(for_table("nonexistent").count(), 0);
    }
}
