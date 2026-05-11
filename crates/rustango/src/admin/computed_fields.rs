//! Computed fields — Django-style computed columns on the admin list view.
//!
//! Models declare a computed field by name in `admin(list_display = "…")`
//! alongside the regular column names; the renderer dispatches to a
//! user-supplied closure via the inventory registry. The closure
//! receives the live `sqlx::postgres::PgRow` so it can pull any column
//! it wants and produce pre-escaped display HTML.
//!
//! ## Example
//!
//! ```ignore
//! use sqlx::Row;
//!
//! #[derive(rustango::Model)]
//! #[rustango(table = "cms_post", admin(list_display = "title, word_count, updated_at"))]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::sql::Auto<i64>,
//!     pub title: String,
//!     pub body: String,
//!     pub updated_at: chrono::DateTime<chrono::Utc>,
//! }
//!
//! rustango::register_admin_computed!(
//!     "cms_post",
//!     "word_count",
//!     "Words",
//!     |row| {
//!         let body: String = row.try_get("body").unwrap_or_default();
//!         body.split_whitespace().count().to_string()
//!     }
//! );
//! ```
//!
//! The list view will show a "Words" column populated by the closure.
//! Names that collide with declared fields lose — the column takes
//! precedence, the computed field is ignored.

use sqlx::postgres::PgRow;

/// Function signature a computed field implements. Receives the raw
/// `PgRow` (no struct round-trip — saves the deserialize when the
/// model has a hundred columns) and returns the pre-escaped HTML to
/// drop into the cell.
pub type ComputedFieldRenderFn = fn(&PgRow) -> String;

/// One computed-field registration. Inventory-collected; submit one
/// per `register_admin_computed!` invocation.
pub struct ComputedField {
    /// SQL table name the field applies to — must match
    /// `ModelSchema::table` exactly.
    pub table: &'static str,
    /// Identifier used in `admin(list_display = "…")`. Must not
    /// collide with a declared field name (declared fields win).
    pub name: &'static str,
    /// Display label shown in the column header. Empty string falls
    /// back to `name`.
    pub label: &'static str,
    /// Renderer. Pure HTML out — caller is responsible for any
    /// escaping needed.
    pub render: ComputedFieldRenderFn,
}

inventory::collect!(ComputedField);

/// Return every computed field registered for `table`. Cheap; the
/// inventory iterator is `O(N)` over all registrations but `N` is
/// small (bounded by the number of computed columns declared across
/// the whole binary).
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static ComputedField> {
    inventory::iter::<ComputedField>
        .into_iter()
        .filter(|m| m.table == table)
        .collect()
}

/// Lookup a single computed field by `(table, name)`.
#[must_use]
pub fn find(table: &str, name: &str) -> Option<&'static ComputedField> {
    inventory::iter::<ComputedField>
        .into_iter()
        .find(|m| m.table == table && m.name == name)
}

/// Register an admin computed field. Pair with a `#[derive(Model)]`
/// type whose `admin(list_display = "…")` names this field.
///
/// ```ignore
/// rustango::register_admin_computed!(
///     "cms_post",            // ModelSchema::table
///     "word_count",          // identifier in list_display
///     "Words",               // column header
///     |row| {
///         use sqlx::Row;
///         let body: String = row.try_get("body").unwrap_or_default();
///         body.split_whitespace().count().to_string()
///     }
/// );
/// ```
#[macro_export]
macro_rules! register_admin_computed {
    ($table:expr, $name:expr, $label:expr, $render:expr) => {
        $crate::inventory::submit! {
            $crate::admin::computed_fields::ComputedField {
                table: $table,
                name: $name,
                label: $label,
                render: $render,
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_compiles_with_zero_entries() {
        // No `register_admin_computed!` in this test binary → empty
        // iter. The point is the inventory link doesn't panic when
        // nothing's submitted.
        let v = for_table("nonexistent_table");
        assert!(v.is_empty());
        let m = find("nonexistent_table", "anything");
        assert!(m.is_none());
    }
}
