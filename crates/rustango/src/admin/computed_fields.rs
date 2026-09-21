//! Computed columns for the admin list view.
//!
//! A model names a computed field in `admin(list_display = "…")` next
//! to its real columns, and the renderer calls a closure from the
//! inventory registry. The closure gets the row as a
//! `serde_json::Value` map of `{ field_name: value }`, the same on
//! every backend, and returns display HTML that is already escaped.
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
//! The list view then shows a "Words" column filled by the closure. A
//! declared field wins any name clash: the computed field is ignored.

/// Renderer for a computed field. Takes the row as a
/// `serde_json::Value` map and returns the cell's HTML, already
/// escaped.
pub type ComputedFieldRenderFn = fn(&serde_json::Value) -> String;

/// Optional link callable for a computed field. Takes the same row
/// JSON as the renderer and returns the cell's link target, or `None`
/// to render the cell without a link.
pub type ComputedFieldLinkFn = fn(&serde_json::Value) -> Option<String>;

/// One computed-field registration, collected by inventory. Each
/// `register_admin_computed!` submits one.
pub struct ComputedField {
    /// SQL table the field belongs to. Must equal `ModelSchema::table`.
    pub table: &'static str,
    /// Name used in `admin(list_display = "…")`. A declared field with
    /// the same name wins.
    pub name: &'static str,
    /// Column header. Empty falls back to `name`.
    pub label: &'static str,
    /// Renderer. Its output goes into the cell as HTML, so it must do
    /// its own escaping.
    pub render: ComputedFieldRenderFn,
    /// Optional per-row link. On `Some(url)` the list view wraps the
    /// cell in `<a href="{url}">…</a>`. On `None`, the default,
    /// `list_display_links` decides whether the cell links.
    pub link: Option<ComputedFieldLinkFn>,
}

inventory::collect!(ComputedField);

/// Every computed field registered for `table`. The scan is `O(N)`
/// over all registrations in the binary, which stays small.
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static ComputedField> {
    inventory::iter::<ComputedField>
        .into_iter()
        .filter(|m| m.table == table)
        .collect()
}

/// Find one computed field by `(table, name)`.
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
/// Pass `link = |row| Option<String>` to give the cell a click target.
/// On `Some(url)` the list view wraps the cell in
/// `<a href="{url}">…</a>`.
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
    // 4-arg form: no link, so the `link` field stays `None`.
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
    // 5-arg form, with a `link = …` callable. The expression must be a
    // `fn(&serde_json::Value) -> Option<String>`; a closure that
    // captures nothing coerces to that fn pointer.
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
        // This test binary registers nothing, so the iterator is
        // empty. The point is that it does not panic.
        let v = for_table("nonexistent_table");
        assert!(v.is_empty());
        let m = find("nonexistent_table", "anything");
        assert!(m.is_none());
    }
}
