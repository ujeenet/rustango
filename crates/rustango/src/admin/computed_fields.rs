//! Computed fields — Django-style computed columns on the admin list view.
//!
//! Models declare a computed field by name in `admin(list_display = "…")`
//! alongside the regular column names; the renderer dispatches to a
//! user-supplied closure via the inventory registry. The closure
//! receives the row as a `serde_json::Value` (a `{ field_name: value }`
//! map produced by [`crate::sql::select_rows_as_json`] — works
//! the same on Postgres / MySQL / SQLite) so it can pull any column
//! it wants and produce pre-escaped display HTML.
//!
//! ## v0.36 breaking change
//!
//! Pre-v0.36 the closure received `&sqlx::postgres::PgRow`. v0.36
//! switched to `&serde_json::Value` so admin's row-rendering path is
//! tri-dialect (the closure no longer cares which backend produced
//! the row). Migration: replace
//! `let body: String = row.try_get("body").unwrap_or_default();`
//! with `let body = row.get("body").and_then(|v| v.as_str()).unwrap_or_default();`.
//!
//! ## Example
//!
//! ```ignore
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
//!         let body = row.get("body").and_then(|v| v.as_str()).unwrap_or_default();
//!         body.split_whitespace().count().to_string()
//!     }
//! );
//! ```
//!
//! The list view will show a "Words" column populated by the closure.
//! Names that collide with declared fields lose — the column takes
//! precedence, the computed field is ignored.

/// Function signature a computed field implements. Receives the row
/// as a `serde_json::Value` (a `{ field_name → value }` map) and
/// returns the pre-escaped HTML to drop into the cell.
///
/// v0.36: switched from `fn(&PgRow) -> String` to
/// `fn(&serde_json::Value) -> String` for tri-dialect admin. The
/// JSON map is produced by [`crate::sql::row_to_json`] /
/// `row_to_json_my` / `row_to_json_sqlite` per the active backend.
pub type ComputedFieldRenderFn = fn(&serde_json::Value) -> String;

/// Function signature a computed field's optional link callable
/// implements. Receives the same row JSON the renderer does and
/// returns the cell's link target URL — `None` to render the cell
/// inline. Issue #349 — Django parity for `list_display` callables
/// that advertise a click target (e.g. an FK detail page).
pub type ComputedFieldLinkFn = fn(&serde_json::Value) -> Option<String>;

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
    /// Issue #349 — optional per-row link callable. When present and
    /// returns `Some(url)`, the admin list view wraps the rendered
    /// cell in `<a href="{url}">…</a>`. When `None` (the default)
    /// the cell behaves exactly as before, deferring to the
    /// container-level `list_display_links` for whether to link.
    pub link: Option<ComputedFieldLinkFn>,
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
///         let body = row.get("body").and_then(|v| v.as_str()).unwrap_or_default();
///         body.split_whitespace().count().to_string()
///     }
/// );
/// ```
///
/// Issue #349 — pass `link = |row| Option<String>` to advertise a
/// per-row click target. When `Some(url)` comes back, the admin
/// list view wraps the rendered cell in `<a href="{url}">…</a>`.
///
/// ```ignore
/// rustango::register_admin_computed!(
///     "cms_post",
///     "author_link",
///     "Author",
///     |row| {
///         row.get("author")
///             .and_then(|a| a.get("name"))
///             .and_then(|v| v.as_str())
///             .unwrap_or("—")
///             .to_string()
///     },
///     link = |row| {
///         row.get("author")
///             .and_then(|a| a.get("id"))
///             .and_then(|v| v.as_i64())
///             .map(|id| format!("/__admin/auth_user/{id}"))
///     },
/// );
/// ```
#[macro_export]
macro_rules! register_admin_computed {
    // 4-arg form — no link. Backwards-compatible with every existing
    // call site; the new `link` field stays `None`.
    ($table:expr, $name:expr, $label:expr, $render:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::admin::computed_fields::ComputedField {
                table: $table,
                name: $name,
                label: $label,
                render: $render,
                link: ::core::option::Option::None,
            }
        }
    };
    // 5-arg form — with explicit `link = …` callable. The link
    // expression must be of type `fn(&serde_json::Value) -> Option<String>`
    // (typically a closure; rust will coerce a non-capturing one to
    // the fn-pointer type expected by `ComputedFieldLinkFn`).
    (
        $table:expr,
        $name:expr,
        $label:expr,
        $render:expr,
        link = $link:expr $(,)?
    ) => {
        $crate::inventory::submit! {
            $crate::admin::computed_fields::ComputedField {
                table: $table,
                name: $name,
                label: $label,
                render: $render,
                link: ::core::option::Option::Some($link),
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
