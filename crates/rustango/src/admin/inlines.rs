//! Admin inlines — Django's `TabularInline` / `StackedInline` shape
//! for editing a model's children inside its parent detail page.
//! Issue #50.
//!
//! ## Status — slice 1 (this module)
//!
//! Read-only display: child rows show up at the bottom of the parent's
//! admin detail page, grouped by inline. Each child links to its own
//! admin row.
//!
//! Edit / add / delete via in-page forms (the FormSet-driven POST
//! handler) is the next slice — the [`InlineAdmin`] struct already
//! exposes `extra`, `max_num`, `readonly_fields` etc. so registrations
//! written today don't need to change when the editor lands.
//!
//! ## Registering an inline
//!
//! ```ignore
//! use rustango::register_admin_inline;
//! use rustango::admin::inlines::InlineKind;
//!
//! #[derive(rustango::Model)]
//! #[rustango(table = "blog_post")]
//! pub struct Post { /* ... */ }
//!
//! #[derive(rustango::Model)]
//! #[rustango(table = "blog_comment")]
//! pub struct Comment {
//!     #[rustango(fk = "blog_post", on = "id")]
//!     pub post_id: i64,
//!     pub body: String,
//! }
//!
//! register_admin_inline!(
//!     parent = "blog_post",       // ModelSchema::table of the parent
//!     child  = "blog_comment",    // ModelSchema::table of the child
//!     fk     = "post_id",         // child column that points back
//!     kind   = InlineKind::Tabular,
//!     label  = "Comments",
//!     fields = &["body", "created_at"],
//!     extra  = 0,
//!     max_num = None,
//!     readonly_fields = &[],
//! );
//! ```
//!
//! The parent's detail page (`/__admin/blog_post/<pk>`) will render a
//! "Comments" panel below the parent fields, listing every
//! `blog_comment` row whose `post_id` matches.
//!
//! Multiple inlines per parent are supported — each registration
//! produces a separate panel, in registration order.

use crate::core::{
    FieldSchema, Filter, ModelEntry, ModelSchema, NullsOrder, Op, OrderItem, SelectQuery, SqlValue,
    WhereExpr,
};
use crate::sql::{select_rows_as_json, ExecError, Pool};

// ============================================================ InlineKind

/// Render variant — Django's two built-in inline shapes.
///
/// * [`InlineKind::Tabular`] — one HTML `<table>` row per child,
///   columns left-to-right (compact; ideal for short rows).
/// * [`InlineKind::Stacked`] — one `<fieldset>` per child with each
///   field on its own line (verbose; ideal for long/multi-line rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineKind {
    /// Table-row layout — Django's `TabularInline`.
    Tabular,
    /// Block layout — Django's `StackedInline`.
    Stacked,
}

// ============================================================ InlineAdmin

/// One inline-admin registration. Inventory-collected; submit one per
/// [`register_admin_inline!`] invocation.
///
/// Fields mirror Django's `InlineModelAdmin` knobs. Fields the v1
/// read-only renderer doesn't consult yet (`extra`, `max_num`,
/// `readonly_fields`) are wired through to the struct so call-sites
/// don't have to change when the FormSet-backed editor lands.
pub struct InlineAdmin {
    /// Parent model's SQL table name — must match
    /// [`ModelSchema::table`] exactly.
    pub parent_table: &'static str,
    /// Child model's SQL table name.
    pub child_table: &'static str,
    /// Column on the child table that points back to the parent's PK.
    /// Must exist on the child model's schema and have an FK relation
    /// to the parent table.
    pub fk_column: &'static str,
    /// Tabular or stacked render variant.
    pub kind: InlineKind,
    /// Panel header. Falls back to the child model's `name` when
    /// empty.
    pub label: &'static str,
    /// Child fields to render, in order. Empty slice means "every
    /// scalar field except the FK column" (mirrors Django's default).
    pub fields: &'static [&'static str],
    /// Number of blank rows the editor offers for creating new
    /// children. Read-only display ignores this; the FormSet editor
    /// will append `extra` empty rows below the existing ones.
    pub extra: usize,
    /// Upper bound on total inline rows (existing + new). `None` =
    /// unlimited. Honored by the FormSet editor.
    pub max_num: Option<usize>,
    /// Field names rendered as plain text even in edit mode. Subset
    /// of `fields`. Read-only display treats every field as readonly.
    pub readonly_fields: &'static [&'static str],
}

inventory::collect!(InlineAdmin);

/// Every inline registered against `parent_table`, in declaration
/// order. Cheap — the inventory iterator is `O(N)` over all
/// registrations but `N` is bounded by the number of admin inlines
/// declared in the whole binary.
#[must_use]
pub fn for_parent_table(parent_table: &str) -> Vec<&'static InlineAdmin> {
    inventory::iter::<InlineAdmin>
        .into_iter()
        .filter(|i| i.parent_table == parent_table)
        .collect()
}

// ============================================================ Registration macro

/// Register an admin inline. The `parent` / `child` arguments must
/// match the `ModelSchema::table` of two `#[derive(Model)]` types
/// already in the registry.
///
/// All optional keys can be omitted — the defaults mirror Django:
/// `kind = InlineKind::Tabular`, `label = ""` (falls back to child
/// model name), `fields = &[]` (every scalar except the FK column),
/// `extra = 0`, `max_num = None`, `readonly_fields = &[]`.
///
/// ```ignore
/// rustango::register_admin_inline!(
///     parent = "blog_post",
///     child  = "blog_comment",
///     fk     = "post_id",
/// );
/// ```
#[macro_export]
macro_rules! register_admin_inline {
    (
        parent = $parent:expr,
        child = $child:expr,
        fk = $fk:expr
        $(, kind = $kind:expr)?
        $(, label = $label:expr)?
        $(, fields = $fields:expr)?
        $(, extra = $extra:expr)?
        $(, max_num = $max_num:expr)?
        $(, readonly_fields = $ro:expr)?
        $(,)?
    ) => {
        $crate::inventory::submit! {
            $crate::admin::inlines::InlineAdmin {
                parent_table: $parent,
                child_table: $child,
                fk_column: $fk,
                kind: $crate::register_admin_inline!(@or $($kind)?; $crate::admin::inlines::InlineKind::Tabular),
                label: $crate::register_admin_inline!(@or $($label)?; ""),
                fields: $crate::register_admin_inline!(@or $($fields)?; &[]),
                extra: $crate::register_admin_inline!(@or $($extra)?; 0usize),
                max_num: $crate::register_admin_inline!(@or $($max_num)?; ::core::option::Option::<usize>::None),
                readonly_fields: $crate::register_admin_inline!(@or $($ro)?; &[]),
            }
        }
    };
    (@or $given:expr; $default:expr) => { $given };
    (@or ; $default:expr) => { $default };
}

// ============================================================ Render helpers

/// One inline panel ready for the Tera detail template. Built per
/// parent-row render via [`render_for_parent`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct InlinePanel {
    /// Panel header text.
    pub label: String,
    /// Child model's table — used to build per-row edit links.
    pub child_table: String,
    /// `"tabular"` or `"stacked"` — selects the template branch.
    pub kind: String,
    /// Column headers, in render order.
    pub field_labels: Vec<String>,
    /// One entry per child row. Each row is a list of pre-rendered
    /// `{label, value}` cells matching `field_labels` order, plus a
    /// `pk` string for the row link.
    pub rows: Vec<serde_json::Value>,
    /// `true` when zero children exist; templates show a "no rows"
    /// placeholder.
    pub empty: bool,
}

/// Resolve every inline registered against `parent_model`, fetch the
/// matching child rows, and return one [`InlinePanel`] per inline.
/// Returns an empty `Vec` when the parent has no registered inlines.
///
/// # Errors
/// [`ExecError`] if a child SELECT fails — caller decides whether to
/// surface or swallow (the admin detail view swallows so one broken
/// inline doesn't take down the whole page).
pub async fn render_for_parent(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
) -> Result<Vec<InlinePanel>, ExecError> {
    let registrations = for_parent_table(parent_model.table);
    if registrations.is_empty() {
        return Ok(Vec::new());
    }

    let mut panels = Vec::with_capacity(registrations.len());
    for inline in registrations {
        let Some(child_model) = find_model_by_table(inline.child_table) else {
            continue;
        };
        // Validate the FK column exists on the child model.
        if child_model.field_by_column(inline.fk_column).is_none() {
            continue;
        }

        let display_fields = resolve_render_fields(child_model, inline);
        // The SELECT must include the PK so we can render a per-row
        // edit link even when the operator's `fields = &[...]` list
        // doesn't include it. Build a projection that prepends the PK
        // when it's not already in the display list.
        let pk_field = child_model.primary_key();
        let select_fields: Vec<&'static FieldSchema> = match pk_field {
            Some(pk) if !display_fields.iter().any(|f| f.column == pk.column) => {
                let mut v = Vec::with_capacity(display_fields.len() + 1);
                v.push(pk);
                v.extend_from_slice(&display_fields);
                v
            }
            _ => display_fields.clone(),
        };
        let order_pk: Vec<OrderItem> = pk_field
            .map(|pk| OrderItem::Column {
                column: pk.column,
                desc: false,
                nulls: NullsOrder::Default,
            })
            .into_iter()
            .collect();

        let rows = select_rows_as_json(
            pool,
            &SelectQuery {
                model: child_model,
                where_clause: WhereExpr::Predicate(Filter {
                    column: inline.fk_column,
                    op: Op::Eq,
                    value: parent_pk.clone(),
                }),
                search: None,
                joins: vec![],
                order_by: order_pk,
                limit: None,
                offset: None,
                lock_mode: None,
                compound: vec![],
                projection: None,
            },
            &select_fields,
        )
        .await?;

        let field_labels = display_fields.iter().map(|f| f.name.to_owned()).collect();
        let pk_column = pk_field.map(|p| p.column).unwrap_or("id");
        let rendered_rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let cells: Vec<serde_json::Value> = display_fields
                    .iter()
                    .map(|f| {
                        let raw = row
                            .get(f.column)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "label": f.name,
                            "value": render_cell_text(&raw),
                        })
                    })
                    .collect();
                let pk_text = row.get(pk_column).map(stringify_pk).unwrap_or_default();
                serde_json::json!({
                    "pk": pk_text,
                    "cells": cells,
                })
            })
            .collect();

        let label = if inline.label.is_empty() {
            child_model.name.to_owned()
        } else {
            inline.label.to_owned()
        };
        let empty = rendered_rows.is_empty();

        panels.push(InlinePanel {
            label,
            child_table: child_model.table.to_owned(),
            kind: match inline.kind {
                InlineKind::Tabular => "tabular".to_owned(),
                InlineKind::Stacked => "stacked".to_owned(),
            },
            field_labels,
            rows: rendered_rows,
            empty,
        });
    }
    Ok(panels)
}

/// Walk the model registry for a schema whose `table` matches.
fn find_model_by_table(table: &str) -> Option<&'static ModelSchema> {
    inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == table)
        .map(|e| e.schema)
}

/// Resolve the field list for an inline. Empty `fields` slice falls
/// back to every scalar field except the FK column (Django default —
/// the FK is implicit, no need to repeat it on every row).
fn resolve_render_fields(
    child_model: &'static ModelSchema,
    inline: &InlineAdmin,
) -> Vec<&'static FieldSchema> {
    if inline.fields.is_empty() {
        return child_model
            .scalar_fields()
            .filter(|f| f.column != inline.fk_column)
            .collect();
    }
    inline
        .fields
        .iter()
        .filter_map(|name| child_model.field(name))
        .collect()
}

/// Convert a JSON value into a short display string. Mirrors what the
/// list-view cell renderer does for unknown-type cells — strings
/// pass through, primitives stringify, complex values get debug-printed
/// so the operator at least sees something instead of a blank cell.
fn render_cell_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => html_escape(s),
        other => html_escape(&other.to_string()),
    }
}

/// Stringify a JSON PK for use in a URL. Numbers and strings only —
/// any other shape returns empty (the row's edit link will simply not
/// render, which is the right failure mode).
fn stringify_pk(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Minimal HTML-escape — pre-rendered cells are dropped into the
/// detail template with `| safe` so untrusted strings need escaping
/// here. Mirrors the helper the list view uses.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_quotes_and_brackets() {
        assert_eq!(
            html_escape("<a href='x'>&"),
            "&lt;a href=&#39;x&#39;&gt;&amp;"
        );
    }

    #[test]
    fn render_cell_text_handles_primitives() {
        assert_eq!(render_cell_text(&serde_json::Value::Null), "");
        assert_eq!(render_cell_text(&serde_json::json!(42)), "42");
        assert_eq!(render_cell_text(&serde_json::json!(true)), "true");
        assert_eq!(
            render_cell_text(&serde_json::json!("hi <b>")),
            "hi &lt;b&gt;"
        );
    }

    #[test]
    fn stringify_pk_supports_numeric_and_string_keys() {
        assert_eq!(stringify_pk(&serde_json::json!(7)), "7");
        assert_eq!(stringify_pk(&serde_json::json!("INV-1")), "INV-1");
        assert_eq!(stringify_pk(&serde_json::json!(null)), "");
    }
}
