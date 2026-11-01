//! Admin inlines: edit a model's children inside the parent's page,
//! in either a tabular or a stacked layout.
//!
//! The detail page shows child rows read-only, one panel per inline,
//! each row linking to its own admin page. The edit page renders the
//! same children as a FormSet, and the parent's update POST applies
//! the submitted inserts, updates and deletes.
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
//! The parent's detail page (`/__admin/blog_post/<pk>`) then renders
//! a "Comments" panel below the parent fields with every
//! `blog_comment` row whose `post_id` matches.
//!
//! A parent can have several inlines. Each registration becomes its
//! own panel, in registration order.

use crate::core::{
    FieldSchema, Filter, ModelEntry, ModelSchema, NullsOrder, Op, OrderItem, SelectQuery, SqlValue,
    WhereExpr, WriteKind,
};
use crate::sql::{select_rows_as_json, ExecError, Pool};
use std::collections::{HashMap, HashSet};

use super::errors::AdminError;
use super::helpers::{admin_config_or_default, is_secret_field, lookup_model};
use super::queryset_hooks::RowScope;
use super::urls::AppState;
use axum::http::request::Parts;

// ============================================================ InlineKind

/// Render variant — the two inline layouts.
///
/// * [`InlineKind::Tabular`]: one `<table>` row per child. Compact,
///   good for short rows.
/// * [`InlineKind::Stacked`]: one `<fieldset>` per child, each field
///   on its own line. Good for long or multi-line rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineKind {
    /// Table-row layout: one row per child.
    Tabular,
    /// Block layout: one labelled block per child.
    Stacked,
}

// ============================================================ InlineAdmin

/// One inline-admin registration, collected by inventory.
pub struct InlineAdmin {
    /// Parent model's SQL table. Must equal [`ModelSchema::table`].
    pub parent_table: &'static str,
    /// Child model's SQL table name.
    pub child_table: &'static str,
    /// Child column pointing back at the parent's PK. It must exist
    /// on the child schema with an FK relation to the parent table.
    pub fk_column: &'static str,
    /// Tabular or stacked render variant.
    pub kind: InlineKind,
    /// Panel header. Falls back to the child model's `name`.
    pub label: &'static str,
    /// Child fields to render, in order. An empty slice means every
    /// scalar field except the FK column.
    pub fields: &'static [&'static str],
    /// Blank rows the edit form adds below the existing ones for
    /// creating new children. The read-only panel ignores it.
    pub extra: usize,
    /// Cap on total rows, existing plus new. `None` means no cap.
    /// It renders as `MAX_NUM_FORMS`; a POST that adds rows past it is refused.
    pub max_num: Option<usize>,
    /// Field names to render as plain text in edit mode, a subset of
    /// `fields`. The POST never writes them.
    pub readonly_fields: &'static [&'static str],
}

inventory::collect!(InlineAdmin);

/// `true` when `column` ties `child_table` rows to an inline's parent,
/// the filter an inline's "edit the others" link sets.
pub(crate) fn is_parent_pin(child_table: &str, column: &str) -> bool {
    inventory::iter::<InlineAdmin>
        .into_iter()
        .any(|i| i.child_table == child_table && i.fk_column == column)
        || inventory::iter::<InlineAdminGeneric>.into_iter().any(|i| {
            i.child_table == child_table && (i.ct_column == column || i.pk_column == column)
        })
}

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
/// All optional keys can be omitted. The defaults are
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

// ============================================================ InlineAdminGeneric (issue #242)

/// One **generic** inline-admin registration. Mirrors [`InlineAdmin`]
/// but keys the relation on a `(ct_column, pk_column)` pair instead
/// of a single FK column, for a generic foreign key.
///
/// The child table carries the GFK; the inline appears on the
/// **parent**'s admin detail page and lists every child row whose
/// `(content_type_id, object_pk)` matches this parent. Slice 1
/// (read-only display) is the foundation that #243 will build the
/// editable / FormSet POST handler on top of.
pub struct InlineAdminGeneric {
    /// Parent model's SQL table — must match [`ModelSchema::table`].
    pub parent_table: &'static str,
    /// Child model's SQL table — the table carrying the GFK columns.
    pub child_table: &'static str,
    /// Child column holding the FK to `rustango_content_types.id`.
    pub ct_column: &'static str,
    /// Child column holding the target row's primary key.
    pub pk_column: &'static str,
    /// Tabular or stacked render variant.
    pub kind: InlineKind,
    /// Panel header. Falls back to the child model's `name`.
    pub label: &'static str,
    /// Child fields to render. Empty = every scalar except the GFK
    /// columns (mirrors `InlineAdmin`'s default).
    pub fields: &'static [&'static str],
    /// Blank rows offered for adding new children — honored by the
    /// editor in #243.
    pub extra: usize,
    /// Upper bound on total rows. Wired through to the management
    /// form; enforced by #243's POST handler.
    pub max_num: Option<usize>,
    /// Field names the edit form shows as plain text and the POST never writes.
    pub readonly_fields: &'static [&'static str],
}

inventory::collect!(InlineAdminGeneric);

/// Every generic inline registered against `parent_table`, in
/// declaration order.
#[must_use]
pub fn generic_for_parent_table(parent_table: &str) -> Vec<&'static InlineAdminGeneric> {
    inventory::iter::<InlineAdminGeneric>
        .into_iter()
        .filter(|i| i.parent_table == parent_table)
        .collect()
}

/// Register a generic admin inline. Same shape as
/// [`register_admin_inline!`] but takes `ct` + `pk` instead of `fk`.
///
/// ```ignore
/// rustango::register_admin_inline_generic!(
///     parent = "blog_post",     // ModelSchema::table of the parent
///     child  = "blog_tag",      // ModelSchema::table of the child
///     ct     = "content_type_id", // child's CT column
///     pk     = "object_pk",     // child's PK column
///     kind   = InlineKind::Tabular,
///     label  = "Tags",
///     fields = &["name"],
/// );
/// ```
#[macro_export]
macro_rules! register_admin_inline_generic {
    (
        parent = $parent:expr,
        child = $child:expr,
        ct = $ct:expr,
        pk = $pk:expr
        $(, kind = $kind:expr)?
        $(, label = $label:expr)?
        $(, fields = $fields:expr)?
        $(, extra = $extra:expr)?
        $(, max_num = $max_num:expr)?
        $(, readonly_fields = $ro:expr)?
        $(,)?
    ) => {
        $crate::inventory::submit! {
            $crate::admin::inlines::InlineAdminGeneric {
                parent_table: $parent,
                child_table: $child,
                ct_column: $ct,
                pk_column: $pk,
                kind: $crate::register_admin_inline_generic!(@or $($kind)?; $crate::admin::inlines::InlineKind::Tabular),
                label: $crate::register_admin_inline_generic!(@or $($label)?; ""),
                fields: $crate::register_admin_inline_generic!(@or $($fields)?; &[]),
                extra: $crate::register_admin_inline_generic!(@or $($extra)?; 0usize),
                max_num: $crate::register_admin_inline_generic!(@or $($max_num)?; ::core::option::Option::<usize>::None),
                readonly_fields: $crate::register_admin_inline_generic!(@or $($ro)?; &[]),
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
    let panels = render_for_parent_in(pool, parent_model, parent_pk, None, None).await?;
    Ok(panels.into_iter().map(|(p, _)| p).collect())
}

/// Child rows an admin detail page lists per inline.
pub(crate) const DETAIL_ROWS: usize = 100;

/// [`render_for_parent`] limited to child rows the request's queryset hooks
/// allow and, with `cap`, to that many rows plus the child-list query for the rest.
pub(crate) async fn render_for_parent_in(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
    parts: Option<&Parts>,
    cap: Option<usize>,
) -> Result<Vec<(InlinePanel, Option<String>)>, ExecError> {
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
        let pk_field = child_model.primary_key();
        let order_pk: Vec<OrderItem> = pk_field
            .map(|pk| OrderItem::Column {
                column: pk.column,
                desc: false,
                nulls: NullsOrder::Default,
            })
            .into_iter()
            .collect();

        // One row past `cap` tells whether any are left out.
        let (mut rows, fetched) = visible_child_rows(
            pool,
            SelectQuery {
                order_by: order_pk,
                limit: cap.map(|c| c as i64 + 1),
                ..SelectQuery::by_pk(child_model, inline.fk_column, parent_pk.clone())
            },
            parts,
        )
        .await?;
        let fk_name = child_model
            .field_by_column(inline.fk_column)
            .map_or(inline.fk_column, |f| f.name);
        let more = cap.and_then(|c| {
            cut_rows(
                &mut rows,
                fetched,
                c,
                &[(fk_name, parent_pk.to_display_string())],
            )
        });

        let field_labels = display_fields.iter().map(|f| f.name.to_owned()).collect();
        let child_cfg = admin_config_or_default(child_model);
        let rendered_rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let cells: Vec<serde_json::Value> = display_fields
                    .iter()
                    .map(|f| {
                        let raw = row.get(f.name).cloned().unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "label": f.display_label(),
                            "value": render_cell(&child_cfg, f, &raw),
                        })
                    })
                    .collect();
                let pk_text = row_pk_text(row, pk_field);
                serde_json::json!({
                    "pk": pk_text,
                    // The View link's path segment; a raw `/` or `?` would break it (#2079).
                    "pk_path": crate::url_codec::url_encode(&pk_text),
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

        let panel = InlinePanel {
            label,
            child_table: child_model.table.to_owned(),
            kind: match inline.kind {
                InlineKind::Tabular => "tabular".to_owned(),
                InlineKind::Stacked => "stacked".to_owned(),
            },
            field_labels,
            rows: rendered_rows,
            empty,
        };
        panels.push((panel, more));
    }
    Ok(panels)
}

/// As [`render_for_parent`] but for generic inlines registered via
/// [`register_admin_inline_generic!`]. Each panel lists every child row
/// whose `(content_type_id, object_pk)` matches the parent.
///
/// Resolves the parent's `ContentType` via the inventory registry +
/// `ContentType::get_by_natural_key` (cached after the first call). The
/// resulting WHERE pins both `ct_column` and `pk_column` on the child.
///
/// # Errors
/// As [`render_for_parent`]. ContentType not seeded yields one
/// `MissingPrimaryKey` error; the caller (detail view) swallows it
/// just as it does for regular inlines.
pub async fn render_generic_for_parent(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
) -> Result<Vec<InlinePanel>, ExecError> {
    let panels = render_generic_for_parent_in(pool, parent_model, parent_pk, None, None).await?;
    Ok(panels.into_iter().map(|(p, _)| p).collect())
}

/// [`render_generic_for_parent`] limited as [`render_for_parent_in`] is.
pub(crate) async fn render_generic_for_parent_in(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
    parts: Option<&Parts>,
    cap: Option<usize>,
) -> Result<Vec<(InlinePanel, Option<String>)>, ExecError> {
    let registrations = generic_for_parent_table(parent_model.table);
    if registrations.is_empty() {
        return Ok(Vec::new());
    }
    // Resolve the parent's ContentType id from the schema. Mirrors
    // `ContentType::for_model<T>` but parameterized on
    // `&'static ModelSchema` since the admin view only has the
    // dyn schema at runtime.
    let Some(ct_id) = resolve_ct_id_for_schema(pool, parent_model).await? else {
        // CT not seeded — return empty panels rather than error so the
        // detail view degrades gracefully (matches the regular-inline
        // posture).
        return Ok(Vec::new());
    };
    let parent_pk_i64 = match &parent_pk {
        SqlValue::I64(v) => *v,
        SqlValue::I32(v) => i64::from(*v),
        SqlValue::I16(v) => i64::from(*v),
        // Generic inlines only support integer parent PKs today —
        // matches the `GenericForeignKey { object_pk: i64 }` shape.
        _ => return Ok(Vec::new()),
    };

    let mut panels = Vec::with_capacity(registrations.len());
    for inline in registrations {
        let Some(child_model) = find_model_by_table(inline.child_table) else {
            continue;
        };
        if child_model.field_by_column(inline.ct_column).is_none()
            || child_model.field_by_column(inline.pk_column).is_none()
        {
            continue;
        }

        let display_fields = resolve_render_fields_generic(child_model, inline);
        let pk_field = child_model.primary_key();
        let order_pk: Vec<OrderItem> = pk_field
            .map(|pk| OrderItem::Column {
                column: pk.column,
                desc: false,
                nulls: NullsOrder::Default,
            })
            .into_iter()
            .collect();

        // #562 — composite AND (ct + pk); struct-update over ::new.
        let (mut rows, fetched) = visible_child_rows(
            pool,
            SelectQuery {
                where_clause: WhereExpr::And(vec![
                    WhereExpr::Predicate(Filter {
                        column: inline.ct_column,
                        op: Op::Eq,
                        value: SqlValue::I64(ct_id),
                    }),
                    WhereExpr::Predicate(Filter {
                        column: inline.pk_column,
                        op: Op::Eq,
                        value: SqlValue::I64(parent_pk_i64),
                    }),
                ]),
                order_by: order_pk,
                limit: cap.map(|c| c as i64 + 1),
                ..SelectQuery::new(child_model)
            },
            parts,
        )
        .await?;
        let name_of = |column: &'static str| {
            child_model
                .field_by_column(column)
                .map_or(column, |f| f.name)
        };
        let more = cap.and_then(|c| {
            cut_rows(
                &mut rows,
                fetched,
                c,
                &[
                    (name_of(inline.ct_column), ct_id.to_string()),
                    (name_of(inline.pk_column), parent_pk_i64.to_string()),
                ],
            )
        });

        let field_labels = display_fields.iter().map(|f| f.name.to_owned()).collect();
        let child_cfg = admin_config_or_default(child_model);
        let rendered_rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let cells: Vec<serde_json::Value> = display_fields
                    .iter()
                    .map(|f| {
                        let raw = row.get(f.name).cloned().unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "label": f.display_label(),
                            "value": render_cell(&child_cfg, f, &raw),
                        })
                    })
                    .collect();
                let pk_text = row_pk_text(row, pk_field);
                serde_json::json!({
                    "pk": pk_text,
                    // The View link's path segment; a raw `/` or `?` would break it (#2079).
                    "pk_path": crate::url_codec::url_encode(&pk_text),
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

        let panel = InlinePanel {
            label,
            child_table: child_model.table.to_owned(),
            kind: match inline.kind {
                InlineKind::Tabular => "tabular".to_owned(),
                InlineKind::Stacked => "stacked".to_owned(),
            },
            field_labels,
            rows: rendered_rows,
            empty,
        };
        panels.push((panel, more));
    }
    Ok(panels)
}

/// As [`resolve_render_fields`] but for generic inlines — excludes
/// both `ct_column` and `pk_column` from the default display field
/// list since they're implicit on a generic-inline panel.
fn resolve_render_fields_generic(
    child_model: &'static ModelSchema,
    inline: &InlineAdminGeneric,
) -> Vec<&'static FieldSchema> {
    if inline.fields.is_empty() {
        return child_model
            .scalar_fields()
            .filter(|f| f.column != inline.ct_column && f.column != inline.pk_column)
            .collect();
    }
    inline
        .fields
        .iter()
        .filter_map(|name| child_model.field(name))
        .collect()
}

/// Look up the ContentType id for `parent_model`. Mirrors
/// `ContentType::for_model<T>`'s logic but parameterized on
/// `&'static ModelSchema` so it works from admin code that only sees
/// the schema at runtime.
async fn resolve_ct_id_for_schema(
    pool: &Pool,
    schema: &'static ModelSchema,
) -> Result<Option<i64>, ExecError> {
    let entry = ModelEntry::for_table(schema.table);
    let Some(entry) = entry else {
        return Ok(None);
    };
    let app = entry.resolved_app_label().unwrap_or("project");
    let name = schema.name.to_ascii_lowercase();
    // Cached per database, like every other content-type lookup.
    let ct = crate::contenttypes::ContentType::get_by_natural_key(pool, app, &name).await?;
    Ok(ct.and_then(|c| c.id.get().copied()))
}

/// Walk the model registry for a schema whose `table` matches.
fn find_model_by_table(table: &str) -> Option<&'static ModelSchema> {
    ModelEntry::for_table(table).map(|e| e.schema)
}

/// Resolve the field list for an inline. Empty `fields` slice falls
/// back to every scalar field except the FK column: the FK is
/// implicit, so there is no need to repeat it on every row.
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
/// One read-only cell: a secret shows only whether it is set (#1861).
fn render_cell(cfg: &crate::core::AdminConfig, f: &FieldSchema, raw: &serde_json::Value) -> String {
    if is_secret_field(cfg, f) {
        super::helpers::render_secret_value(Some(raw))
    } else {
        render_cell_text(raw)
    }
}

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

// ============================================================ Editable rendering (slice 2)

/// One editable inline panel for the `form.html` template. Mirrors
/// [`InlinePanel`] but each cell carries pre-rendered `<input>`
/// HTML instead of a static value, plus the FormSet management-form
/// fields and the `<prefix>-N-<field>` naming.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InlineFormPanel {
    /// Panel header text.
    pub label: String,
    /// Child model's SQL table — used by the POST processor to look
    /// up the schema. Also surfaces in the rendered DOM so JS could
    /// scope row-add buttons to this panel.
    pub child_table: String,
    /// `"tabular"` or `"stacked"`.
    pub kind: String,
    /// FormSet prefix used for every input on every row. Stable across
    /// GET + POST (defaults to `child_table`).
    pub prefix: String,
    /// Total form rows (existing + extra blanks). Drives the
    /// `<prefix>-TOTAL_FORMS` management input.
    pub total_forms: usize,
    /// Existing-rows count. Drives `<prefix>-INITIAL_FORMS`.
    pub initial_forms: usize,
    /// `<prefix>-MAX_NUM_FORMS` value; `None` renders as empty
    /// (the "no cap" sentinel).
    pub max_num: Option<usize>,
    /// Column headers, in render order. Padded with a final
    /// `"Delete"` column when at least one existing row is present
    /// (so the delete-checkbox column lines up with the others).
    pub field_labels: Vec<String>,
    /// One entry per form row. Each row carries pre-rendered
    /// `{ label, input_html }` cells, plus `pk`, `hidden_pk` and
    /// `delete_input_html` (all empty for new rows). A posted row not in
    /// the shown list has empty `cells` and an `error` message instead.
    pub rows: Vec<serde_json::Value>,
    /// Child-list query string for the rows past the formset cap;
    /// `None` when every row is shown (#1977).
    pub more_rows_filter: Option<String>,
}

/// Existing rows one panel renders: the formset cap less its blank
/// rows, so the TOTAL_FORMS it renders passes the POST check (#1977).
fn row_window(extra: usize) -> (usize, usize) {
    let extra = extra.min(crate::forms::formset::MAX_FORMS);
    (crate::forms::formset::MAX_FORMS - extra, extra)
}

/// Keep the first `cap` rows; the filter links the child list when the
/// fetch, before the view hook, held more.
fn cut_rows(
    rows: &mut Vec<serde_json::Value>,
    fetched: usize,
    cap: usize,
    filter: &[(&str, String)],
) -> Option<String> {
    rows.truncate(cap);
    if fetched <= cap {
        return None;
    }
    serde_urlencoded::to_string(filter).ok()
}

/// As [`render_for_parent`] but produces an editable [`InlineFormPanel`]
/// — N existing-row inputs prefilled, then `extra` blank rows below.
/// Each existing row carries a hidden `<prefix>-N-<pk>` input + a
/// `<prefix>-N-DELETE` checkbox so the POST handler can identify which
/// rows to UPDATE vs DELETE.
///
/// # Errors
/// As [`render_for_parent`].
pub async fn render_form_for_parent(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
) -> Result<Vec<InlineFormPanel>, ExecError> {
    render_form_for_parent_in(pool, parent_model, parent_pk, None, None).await
}

/// [`render_form_for_parent`] limited to child rows the request's queryset
/// hooks allow, with the fields it may not write locked.
pub(crate) async fn render_form_for_parent_in(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
    req: Option<(&AppState, &Parts)>,
    submitted: Option<&HashMap<String, String>>,
) -> Result<Vec<InlineFormPanel>, ExecError> {
    let parts = req.map(|(_, p)| p);
    let registrations = for_parent_table(parent_model.table);
    if registrations.is_empty() {
        return Ok(Vec::new());
    }

    let mut panels = Vec::with_capacity(registrations.len());
    for inline in registrations {
        let Some(child_model) = find_model_by_table(inline.child_table) else {
            continue;
        };
        if child_model.field_by_column(inline.fk_column).is_none() {
            continue;
        }

        let display_fields = resolve_render_fields(child_model, inline);
        let pk_field = child_model.primary_key();
        let order_pk: Vec<OrderItem> = pk_field
            .map(|pk| OrderItem::Column {
                column: pk.column,
                desc: false,
                nulls: NullsOrder::Default,
            })
            .into_iter()
            .collect();

        // One row past the window tells whether any are left out.
        let (cap, extra) = row_window(inline.extra);
        let (mut rows, fetched) = visible_child_rows(
            pool,
            SelectQuery {
                order_by: order_pk,
                limit: Some(cap as i64 + 1),
                ..SelectQuery::by_pk(child_model, inline.fk_column, parent_pk.clone())
            },
            parts,
        )
        .await?;
        let fk_name = child_model
            .field_by_column(inline.fk_column)
            .map_or(inline.fk_column, |f| f.name);
        let more_rows_filter = cut_rows(
            &mut rows,
            fetched,
            cap,
            &[(fk_name, parent_pk.to_display_string())],
        );

        let pins = [inline.fk_column];
        let prefix = formset_prefix(parent_model.table, child_model.table, &pins);
        let initial_forms = rows.len();
        let child_cfg = admin_config_or_default(child_model);
        let locked = inline_locks(
            child_model,
            inline.readonly_fields,
            &pins,
            req.map(|(s, _)| s),
        );
        let (total_forms, rendered_rows) = form_panel_rows(
            &child_cfg,
            &display_fields,
            pk_field,
            (&prefix, &locked),
            &rows,
            extra,
            submitted,
        );

        let mut field_labels: Vec<String> =
            display_fields.iter().map(|f| f.name.to_owned()).collect();
        // Append a Delete column only when there's something to delete.
        if initial_forms > 0 {
            field_labels.push("Delete".to_owned());
        }

        let label = if inline.label.is_empty() {
            child_model.name.to_owned()
        } else {
            inline.label.to_owned()
        };

        panels.push(InlineFormPanel {
            label,
            child_table: child_model.table.to_owned(),
            kind: match inline.kind {
                InlineKind::Tabular => "tabular".to_owned(),
                InlineKind::Stacked => "stacked".to_owned(),
            },
            prefix,
            total_forms,
            initial_forms,
            max_num: inline.max_num,
            field_labels,
            rows: rendered_rows,
            more_rows_filter,
        });
    }
    Ok(panels)
}

/// Rendered rows of one editable panel and its TOTAL_FORMS. With a
/// refused POST in `submitted`, the rows show what was typed, not the stored
/// values. Typed rows are matched to stored ones by PK; a typed row whose
/// row is gone shows as an error, so its values land on no other row.
fn form_panel_rows(
    child_cfg: &crate::core::AdminConfig,
    display_fields: &[&'static FieldSchema],
    pk_field: Option<&'static FieldSchema>,
    (prefix, locked): (&str, &[&str]),
    rows: &[serde_json::Value],
    extra: usize,
    submitted: Option<&HashMap<String, String>>,
) -> (usize, Vec<serde_json::Value>) {
    let initial_forms = rows.len();
    let pk_field_name = pk_field.map_or("id", |p| p.name);
    // Only a POST that carried this panel's formset replaces its values.
    let posted = submitted.and_then(|form| {
        let total = crate::forms::formset::total_forms(form, prefix).ok()?;
        Some(PostedSlots::new(form, prefix, pk_field_name, total))
    });
    let total_forms = posted.as_ref().map_or(initial_forms + extra, |p| {
        (initial_forms + p.new.len()).min(crate::forms::formset::MAX_FORMS)
    });
    // `slot` is the posted index; `None` keeps the stored value.
    let typed = |slot: Option<usize>, f: &FieldSchema, kind: WriteKind| -> Option<String> {
        let form = submitted.filter(|_| f.accepts_input(kind) && !locked.contains(&f.name))?;
        // An unchecked box is not sent: missing means blank.
        Some(
            form.get(&format!("{prefix}-{}-{}", slot?, f.name))
                .cloned()
                .unwrap_or_default(),
        )
    };
    let mut rendered_rows: Vec<serde_json::Value> = Vec::with_capacity(total_forms);
    for (idx, row) in rows.iter().enumerate() {
        let pk_text = row_pk_text(row, pk_field);
        let slot = posted.as_ref().and_then(|p| p.slot_of(&pk_text));
        let cells: Vec<serde_json::Value> = display_fields
            .iter()
            .map(|f| {
                // Route through the same formatter the main change form
                // uses, so DateTime/Json/Array values are normalised to
                // what each `<input>` accepts.
                let raw_str = typed(slot, f, WriteKind::Update)
                    .unwrap_or_else(|| crate::admin::render::render_value_for_input_json(row, f));
                let input_html = render_prefixed_input(
                    child_cfg,
                    f,
                    &raw_str,
                    (prefix, idx, WriteKind::Update),
                    locked.contains(&f.name),
                );
                serde_json::json!({
                    "label": f.display_label(),
                    "input_html": input_html,
                })
            })
            .collect();
        // Hidden PK input keeps the row identifiable through the
        // round-trip even if the operator changes nothing else.
        let hidden_pk = format!(
            r#"<input type="hidden" name="{p}-{i}-{n}" value="{v}">"#,
            p = html_escape(prefix),
            i = idx,
            n = html_escape(pk_field_name),
            v = html_escape(&pk_text),
        );
        let deleting = slot.is_some_and(|s| posted.as_ref().is_some_and(|p| p.deleting(s)));
        let delete_input_html = format!(
            r#"<input type="checkbox" name="{p}-{i}-DELETE" value="on"{c}>"#,
            p = html_escape(prefix),
            i = idx,
            c = if deleting { " checked" } else { "" },
        );
        rendered_rows.push(serde_json::json!({
            "pk": pk_text,
            "cells": cells,
            "hidden_pk": hidden_pk,
            "delete_input_html": delete_input_html,
        }));
    }
    // Typed rows not in the fetched window: shown, never posted again.
    if let Some(p) = &posted {
        let shown: HashSet<String> = rows.iter().map(|r| row_pk_text(r, pk_field)).collect();
        for (slot, pk) in &p.existing {
            if shown.contains(pk) || p.deleting(*slot) {
                continue;
            }
            rendered_rows.push(serde_json::json!({
                "pk": pk,
                // Not proof of a delete: the list is capped, and a PK
                // may be posted in another spelling.
                "error": format!(
                    "Row {pk} is not in this list (deleted, or past the rows shown here); \
                     its changes were not saved."
                ),
                "cells": [],
                "hidden_pk": "",
                "delete_input_html": "",
            }));
        }
    }
    // Blank rows for adding new children. No hidden PK (the POST
    // handler treats absence as "INSERT") and no DELETE box.
    for idx in initial_forms..total_forms {
        let slot = posted
            .as_ref()
            .and_then(|p| p.new.get(idx - initial_forms).copied());
        let cells: Vec<serde_json::Value> = display_fields
            .iter()
            .map(|f| {
                let raw_str = typed(slot, f, WriteKind::Insert).unwrap_or_default();
                let input_html = render_prefixed_input(
                    child_cfg,
                    f,
                    &raw_str,
                    (prefix, idx, WriteKind::Insert),
                    locked.contains(&f.name),
                );
                serde_json::json!({
                    "label": f.display_label(),
                    "input_html": input_html,
                })
            })
            .collect();
        rendered_rows.push(serde_json::json!({
            "pk": "",
            "cells": cells,
            "hidden_pk": "",
            "delete_input_html": "",
        }));
    }
    (total_forms, rendered_rows)
}

/// A posted formset's slots, split as [`plan_post`] reads them.
struct PostedSlots<'a> {
    form: &'a HashMap<String, String>,
    prefix: &'a str,
    /// Slots of existing rows, with the PK each was posted for.
    existing: Vec<(usize, String)>,
    /// Slots of new rows, in order.
    new: Vec<usize>,
}

impl<'a> PostedSlots<'a> {
    fn new(
        form: &'a HashMap<String, String>,
        prefix: &'a str,
        pk_name: &str,
        total: usize,
    ) -> Self {
        let initial = crate::forms::formset::initial_forms(form, prefix);
        let (mut existing, mut new) = (Vec::new(), Vec::new());
        for slot in 0..total {
            let pk = form
                .get(&format!("{prefix}-{slot}-{pk_name}"))
                .map_or("", String::as_str);
            if slot_is_new(initial, slot, pk) {
                new.push(slot);
            } else {
                existing.push((slot, pk.trim().to_owned()));
            }
        }
        Self {
            form,
            prefix,
            existing,
            new,
        }
    }

    /// The slot posted for the existing row `pk`.
    fn slot_of(&self, pk: &str) -> Option<usize> {
        self.existing.iter().find(|(_, p)| p == pk).map(|(s, _)| *s)
    }

    fn deleting(&self, slot: usize) -> bool {
        self.form
            .get(&format!("{}-{slot}-DELETE", self.prefix))
            .is_some_and(|v| !v.is_empty())
    }
}

/// Whether posted slot `idx` is a new row: past INITIAL_FORMS, or without
/// it, one with no PK.
fn slot_is_new(initial: Option<usize>, idx: usize, raw_pk: &str) -> bool {
    initial.map_or(raw_pk.trim().is_empty(), |n| idx >= n)
}

/// Wrap `super::render::render_input` so the generated `name=` /
/// `id=` attributes are prefix-mangled into the FormSet
/// `<prefix>-<idx>-<field>` shape. We accomplish this with a
/// `str::replace` on the rendered HTML — `render_input` always emits
/// `name="<field>"` and `id="<field>"`, so the substitution is
/// uniquely targetable.
fn render_prefixed_input(
    cfg: &crate::core::AdminConfig,
    field: &FieldSchema,
    value: &str,
    (prefix, idx, kind): (&str, usize, crate::core::WriteKind),
    locked: bool,
) -> String {
    // A locked, auto or generated column is shown, never input; a secret only as set or not.
    if locked || !field.accepts_input(kind) {
        let shown = if is_secret_field(cfg, field) {
            super::helpers::render_secret_value(Some(&serde_json::Value::String(value.to_owned())))
        } else {
            html_escape(value)
        };
        return format!(r#"<span class="readonly">{shown}</span>"#);
    }
    // A blank add-row value takes the column default, so it may be left empty.
    let field = &FieldSchema {
        blank: field.blank
            || (kind == WriteKind::Insert && crate::forms::blank_takes_default(field, Some(""))),
        ..*field
    };
    // The child's widget overrides apply; a secret is never echoed (#1861).
    let base = if is_secret_field(cfg, field) {
        crate::admin::render::render_secret_input(field, false, false)
    } else {
        let widget = cfg
            .formfield_overrides
            .iter()
            .find(|(name, _)| *name == field.name)
            .map(|(_, w)| *w);
        crate::admin::render::render_input_with_widget(field, value, false, widget)
    };
    let target_name = format!(r#"name="{}""#, field.name);
    let new_name = format!(r#"name="{prefix}-{idx}-{}""#, field.name);
    let target_id = format!(r#"id="{}""#, field.name);
    let new_id = format!(r#"id="{prefix}-{idx}-{}""#, field.name);
    base.replacen(&target_name, &new_name, 1)
        .replacen(&target_id, &new_id, 1)
}

// ============================================================ POST processing (slice 2)

/// Child rows an inline may show: inside the queryset hooks and passing
/// the child's `view` object-permission hook (#1717). `None` parts (the
/// public no-request helpers) leaves them unscoped. Rows carry every
/// scalar field, as the detail view's hooks see them. Also returns how
/// many rows the query fetched before the hook dropped any.
async fn visible_child_rows(
    pool: &Pool,
    query: SelectQuery,
    parts: Option<&Parts>,
) -> Result<(Vec<serde_json::Value>, usize), ExecError> {
    let child = query.model;
    let fields: Vec<&'static FieldSchema> = child.scalar_fields().collect();
    let Some(parts) = parts else {
        let rows = select_rows_as_json(pool, &query, &fields).await?;
        let fetched = rows.len();
        return Ok((rows, fetched));
    };
    let query = SelectQuery {
        where_clause: RowScope::of(child, parts).constrain(query.where_clause),
        ..query
    };
    let mut rows = select_rows_as_json(pool, &query, &fields).await?;
    let fetched = rows.len();
    rows.retain(|row| {
        crate::admin::object_permissions::is_allowed(child.table, "view", parts, Some(row))
    });
    Ok((rows, fetched))
}

/// The columns that tie a child row to its parent, with this parent's values.
struct ParentScope(Vec<(&'static str, SqlValue)>);

impl ParentScope {
    /// `pk = <pk>` AND every pin: a PK under another parent matches nothing.
    fn row_where(&self, pk_column: &'static str, pk: SqlValue) -> WhereExpr {
        let mut filters = vec![Filter::new(pk_column, Op::Eq, pk)];
        filters.extend(self.filters());
        WhereExpr::and_predicates(filters)
    }

    /// Every child row of this parent.
    fn all_where(&self) -> WhereExpr {
        WhereExpr::and_predicates(self.filters().collect())
    }

    fn filters(&self) -> impl Iterator<Item = Filter> + '_ {
        self.0
            .iter()
            .map(|(column, value)| Filter::new(column, Op::Eq, value.clone()))
    }

    fn columns(&self) -> Vec<&'static str> {
        self.0.iter().map(|(c, _)| *c).collect()
    }
}

/// The FormSet prefix of one inline, for render and POST alike: the child
/// table, plus the pin columns when another inline on the parent shows it.
fn formset_prefix(parent_table: &str, child_table: &str, pins: &[&str]) -> String {
    let fk = for_parent_table(parent_table)
        .iter()
        .filter(|i| i.child_table == child_table)
        .count();
    let generic = generic_for_parent_table(parent_table)
        .iter()
        .filter(|i| i.child_table == child_table)
        .count();
    if fk + generic > 1 {
        format!("{child_table}-{}", pins.join("-"))
    } else {
        child_table.to_owned()
    }
}

/// Child fields an inline row never writes: the parent pins, the inline's
/// and the child's read-only fields, and superuser-only ones on add or
/// change. Without a request every superuser-only field is locked.
fn inline_locks(
    child: &'static ModelSchema,
    inline_readonly: &[&'static str],
    pins: &[&str],
    state: Option<&AppState>,
) -> Vec<&'static str> {
    use super::object_permissions::{superuser_fields, AdminWrite};
    let mut out: Vec<&'static str> = child
        .scalar_fields()
        .filter(|f| pins.contains(&f.column))
        .map(|f| f.name)
        .collect();
    out.extend_from_slice(inline_readonly);
    for action in [AdminWrite::Add, AdminWrite::Change] {
        match state {
            Some(state) => out.extend(state.locked_fields(child, action).all()),
            None => {
                out.extend_from_slice(admin_config_or_default(child).readonly_fields);
                out.extend(superuser_fields(child.table, action));
            }
        }
    }
    out
}

/// One inline registration resolved for a parent row.
struct InlineTarget {
    child: &'static ModelSchema,
    /// The FormSet prefix its panel posts under.
    prefix: String,
    pk: &'static FieldSchema,
    /// Fields a row may write: not the PK, a pin, or a read-only field.
    writable: Vec<&'static FieldSchema>,
    scope: ParentScope,
    /// The child table's queryset-hook scope for this request.
    rows: RowScope,
    /// Secret field names: an empty one on an edit keeps the stored value.
    secrets: Vec<&'static str>,
    /// The registration's `max_num`.
    max_num: Option<usize>,
    /// Superuser-only fields on add, which an INSERT defaults.
    add_locked: Vec<&'static str>,
}

impl InlineTarget {
    #[allow(clippy::too_many_arguments)]
    fn new(
        parent_table: &str,
        child: &'static ModelSchema,
        display: &[&'static FieldSchema],
        inline_readonly: &[&'static str],
        max_num: Option<usize>,
        scope: ParentScope,
        state: &AppState,
        parts: &Parts,
    ) -> Option<Self> {
        let pk = child.primary_key()?;
        let cfg = admin_config_or_default(child);
        let pins = scope.columns();
        // An inline row may be added or changed: lock for both.
        let locked = inline_locks(child, inline_readonly, &pins, Some(state));
        let add = state.locked_fields(child, super::object_permissions::AdminWrite::Add);
        let secrets = display
            .iter()
            .filter(|f| is_secret_field(&cfg, f))
            .map(|f| f.name)
            .collect();
        let writable = display
            .iter()
            .copied()
            .filter(|f| {
                f.accepts_input(WriteKind::Update)
                    && f.column != pk.column
                    && !locked.contains(&f.name)
            })
            .collect();
        Some(Self {
            child,
            prefix: formset_prefix(parent_table, child.table, &pins),
            pk,
            writable,
            scope,
            rows: RowScope::of(child, parts),
            secrets,
            max_num,
            add_locked: add.superuser_only,
        })
    }

    /// `pk = <pk>` under this parent and inside the hook scope.
    fn row_where(&self, pk: SqlValue) -> WhereExpr {
        self.rows
            .constrain(self.scope.row_where(self.pk.column, pk))
    }

    /// `edit` drops an empty secret, so the stored one stays; an add
    /// leaves an unsent defaulted field to its column default.
    fn values(
        &self,
        row: &HashMap<String, String>,
        edit: bool,
    ) -> Result<Vec<(&'static str, SqlValue)>, crate::forms::FormError> {
        self.writable
            .iter()
            .filter(|f| {
                if edit {
                    !(self.secrets.contains(&f.name)
                        && row.get(f.name).is_none_or(String::is_empty))
                } else {
                    !crate::forms::blank_takes_default(f, row.get(f.name).map(String::as_str))
                }
            })
            .map(|f| {
                let value = crate::forms::parse_form_value(f, row.get(f.name).map(String::as_str))?;
                Ok((f.column, value))
            })
            .collect()
    }

    /// The stored rows for `pks` in two reads at most: those under
    /// this parent, then which of the rest exist under another.
    async fn fetch_own_many(
        &self,
        pool: &Pool,
        parts: &Parts,
        pks: Vec<SqlValue>,
    ) -> Result<OwnRows, ExecError> {
        let mut out = OwnRows(HashMap::new());
        if pks.is_empty() {
            return Ok(out);
        }
        let in_pks = |pks: Vec<SqlValue>| Filter::new(self.pk.column, Op::In, SqlValue::List(pks));
        let mut filters = vec![in_pks(pks.clone())];
        filters.extend(self.scope.filters());
        let fields: Vec<&'static FieldSchema> = self.child.scalar_fields().collect();
        let query = SelectQuery {
            where_clause: self.rows.constrain(WhereExpr::and_predicates(filters)),
            ..SelectQuery::new(self.child)
        };
        for row in select_rows_as_json(pool, &query, &fields).await? {
            let visible = crate::admin::object_permissions::is_allowed(
                self.child.table,
                "view",
                parts,
                Some(&row),
            );
            let key = pk_key(&row, self.pk);
            out.0
                .insert(key, if visible { Own::Row(row) } else { Own::Foreign });
        }
        let rest: Vec<SqlValue> = pks
            .into_iter()
            .filter(|pk| !out.0.contains_key(&pk.to_display_string()))
            .collect();
        if rest.is_empty() {
            return Ok(out);
        }
        let any_parent = SelectQuery {
            where_clause: WhereExpr::Predicate(in_pks(rest)),
            ..SelectQuery::new(self.child)
        };
        for row in select_rows_as_json(pool, &any_parent, &[self.pk]).await? {
            out.0.insert(pk_key(&row, self.pk), Own::Foreign);
        }
        Ok(out)
    }

    /// `true` when every submitted value equals the stored one, read
    /// the way the edit form renders it. A typed secret is always a
    /// change: comparing it would make the skip a guessing oracle.
    fn unchanged(&self, before: &serde_json::Value, values: &[(&'static str, SqlValue)]) -> bool {
        values.iter().all(|(column, submitted)| {
            let Some(f) = self.writable.iter().find(|f| f.column == *column) else {
                return false;
            };
            if self.secrets.contains(&f.name) {
                return false;
            }
            let stored = crate::admin::render::render_value_for_input_json(before, f);
            crate::forms::parse_form_value(f, Some(&stored)).is_ok_and(|v| v == *submitted)
        })
    }
}

/// A child PK's stored row as [`InlineTarget::fetch_own_many`] read it.
enum Own {
    Row(serde_json::Value),
    /// Under another parent, or hidden by the `view` hook.
    Foreign,
}

/// Prefetched child rows keyed by PK text; an absent key is a gone row.
struct OwnRows(HashMap<String, Own>);

impl OwnRows {
    /// The child row under this parent; `None` when no row has this PK.
    ///
    /// # Errors
    /// [`AdminError::RowNotFound`] when the PK belongs to another parent
    /// or the `view` hook refuses the row.
    fn get(
        &self,
        child: &'static ModelSchema,
        pk: &SqlValue,
        raw_pk: &str,
    ) -> Result<Option<serde_json::Value>, AdminError> {
        match self.0.get(&pk.to_display_string()) {
            Some(Own::Row(row)) => Ok(Some(row.clone())),
            Some(Own::Foreign) => Err(AdminError::RowNotFound {
                table: child.table.to_owned(),
                pk: raw_pk.to_owned(),
            }),
            None => Ok(None),
        }
    }
}

/// A fetched row's PK text; rows are keyed by field name.
fn row_pk_text(row: &serde_json::Value, pk: Option<&FieldSchema>) -> String {
    pk.map_or_else(
        || row.get("id").map(stringify_pk).unwrap_or_default(),
        |pk| pk_key(row, pk),
    )
}

/// A fetched row's PK as [`SqlValue::to_display_string`] spells it.
fn pk_key(row: &serde_json::Value, pk: &FieldSchema) -> String {
    row.get(pk.name)
        .or_else(|| row.get(pk.column))
        .map(stringify_pk)
        .unwrap_or_default()
}

/// An inline row's FK values, checked after the plan loop.
struct PendingLink {
    /// How the refusal names the row: its slot number or its PK.
    row: String,
    values: Vec<(&'static str, SqlValue)>,
    /// An update's PK and stored row.
    stored: Option<(SqlValue, serde_json::Value)>,
}

/// Why [`plan_post`] refused the inline rows.
pub(crate) enum InlinePlanError {
    /// A gate or lookup error: the response is the error's own.
    Admin(AdminError),
    /// An edited child row was deleted after the page loaded, a row
    /// failed to parse (#2339), or the rows would pass `max_num`. The
    /// form re-renders with this message.
    Rejected(String),
    /// A malformed or oversized management form (#1892). The form re-renders.
    BadFormset(crate::forms::formset::FormSetError),
}

impl From<AdminError> for InlinePlanError {
    fn from(e: AdminError) -> Self {
        Self::Admin(e)
    }
}

impl From<ExecError> for InlinePlanError {
    fn from(e: ExecError) -> Self {
        Self::Admin(e.into())
    }
}

/// Inline writes that passed the child tables' admin gates. Every
/// UPDATE and DELETE is keyed on the parent; every INSERT pins it.
pub(crate) struct InlinePlan {
    targets: Vec<TargetPlan>,
    /// The parent row, locked in the save tx before any `max_num` recount.
    parent: (&'static ModelSchema, SqlValue),
}

/// One inline's writes: existing rows first, then inserts.
struct TargetPlan {
    child: &'static ModelSchema,
    pk: &'static FieldSchema,
    existing: Vec<InlineWrite>,
    inserts: Vec<InlineInsert>,
    /// `max_num` and every live row of the parent, recounted in the save tx.
    cap: Option<(usize, WhereExpr)>,
}

/// Every child row is audited, as on its own page.
enum InlineWrite {
    Update {
        query: crate::core::UpdateQuery,
        /// Re-read after the write to check the row is still in scope.
        pk: SqlValue,
        audit: ChildAudit,
        /// An extra host's claim, as on its own page.
        #[cfg(feature = "tenancy")]
        claims: crate::tenancy::org_host::AdminClaims,
    },
    Delete {
        query: InlineRemove,
        /// The entity PK the snapshot is filed under.
        pk: String,
    },
}

/// One child row an inline write touches, for its admin signals.
pub(crate) enum ChildSignal {
    Save {
        table: &'static str,
        pk: String,
        change: bool,
    },
    Delete {
        table: &'static str,
        pk: String,
    },
}

impl ChildSignal {
    /// `admin_pre_*` when `pre`, else `admin_post_*`.
    pub(crate) async fn send(self, pre: bool, state: &AppState) {
        use crate::signals::admin::{self as sig, AdminDeleteContext, AdminSaveContext};
        let tenant = state.config.tenant.clone();
        match self {
            Self::Save { table, pk, change } => {
                let ctx = AdminSaveContext::new(table, pk, change).with_tenant(tenant);
                if pre {
                    sig::send_admin_pre_save(ctx).await;
                } else {
                    sig::send_admin_post_save(ctx).await;
                }
            }
            Self::Delete { table, pk } => {
                let ctx = AdminDeleteContext::new(table, pk).with_tenant(tenant);
                if pre {
                    sig::send_admin_pre_delete(ctx).await;
                } else {
                    sig::send_admin_post_delete(ctx).await;
                }
            }
        }
    }
}

impl InlinePlan {
    /// Extra hosts the plan writes, locked before the save reads anything.
    #[cfg(feature = "tenancy")]
    pub(crate) fn extra_hosts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.targets {
            for w in &t.existing {
                if let InlineWrite::Update { claims, .. } = w {
                    out.extend(claims.extra_host().map(str::to_owned));
                }
            }
            out.extend(
                t.inserts
                    .iter()
                    .filter_map(|i| i.claims.extra_host().map(str::to_owned)),
            );
        }
        out
    }

    /// The rows this plan will write, for `admin_pre_*`; a new row's PK is
    /// empty unless the form typed it.
    pub(crate) fn planned_rows(&self) -> Vec<ChildSignal> {
        let mut out = Vec::new();
        for t in &self.targets {
            let table = t.child.table;
            for w in &t.existing {
                out.push(match w {
                    InlineWrite::Update { pk, .. } => ChildSignal::Save {
                        table,
                        pk: pk.to_display_string(),
                        change: true,
                    },
                    InlineWrite::Delete { pk, .. } => ChildSignal::Delete {
                        table,
                        pk: pk.clone(),
                    },
                });
            }
            for i in &t.inserts {
                let pk = i
                    .query
                    .columns
                    .iter()
                    .position(|c| *c == t.pk.column)
                    .map(|n| i.query.values[n].to_display_string())
                    .unwrap_or_default();
                out.push(ChildSignal::Save {
                    table,
                    pk,
                    change: false,
                });
            }
        }
        out
    }
}

/// A `soft_delete` child is stamped, as the main delete does (#2453).
enum InlineRemove {
    Hard(crate::core::DeleteQuery),
    Soft(crate::core::UpdateQuery),
}

impl InlineRemove {
    fn where_clause(&self) -> &WhereExpr {
        match self {
            Self::Hard(q) => &q.where_clause,
            Self::Soft(q) => &q.where_clause,
        }
    }

    fn op(&self) -> crate::audit::AuditOp {
        match self {
            Self::Hard(_) => crate::audit::AuditOp::Delete,
            Self::Soft(_) => crate::audit::AuditOp::SoftDelete,
        }
    }

    async fn run(&self, tx: &mut crate::sql::PoolTx<'_>) -> Result<u64, ExecError> {
        match self {
            Self::Hard(q) => crate::sql::delete_tx(tx, q).await,
            Self::Soft(q) => crate::sql::update_tx(tx, q).await,
        }
    }
}

struct InlineInsert {
    query: crate::core::InsertQuery,
    /// The audit form of the new row.
    audit: HashMap<String, String>,
    #[cfg(feature = "tenancy")]
    claims: crate::tenancy::org_host::AdminClaims,
}

/// What a child UPDATE's audit entry diffs its locked row against.
struct ChildAudit {
    pk: String,
    form: HashMap<String, String>,
}

/// Check every submitted inline row (FK and generic) against the child
/// table's admin gates and build its write. Writes nothing.
///
/// A row per FormSet slot: a slot past `INITIAL_FORMS` (or, without it, an
/// empty PK) with content → INSERT; an existing row with the DELETE box →
/// DELETE, without it → UPDATE. A row whose values fail to parse
/// refuses the whole POST.
///
/// An unchanged existing row is skipped: no gate, no write. Deleting a
/// row that is already gone counts as done.
///
/// # Errors
/// [`AdminError::ReadOnly`] or [`AdminError::Forbidden`] when a child
/// gate refuses a row; [`AdminError::RowNotFound`] when a submitted child
/// PK is under another parent or hidden by its `view` hook; [`InlinePlanError::Rejected`] when an edited
/// row no longer exists, a row fails to parse, or the rows would pass `max_num`.
pub(crate) async fn plan_post(
    state: &AppState,
    parts: &Parts,
    parent_model: &'static ModelSchema,
    parent_pk: &SqlValue,
    form: &HashMap<String, String>,
) -> Result<InlinePlan, InlinePlanError> {
    let mut plan = InlinePlan {
        targets: Vec::new(),
        parent: (parent_model, parent_pk.clone()),
    };
    for target in inline_targets(state, parts, parent_model, parent_pk).await? {
        plan_target(state, parts, &target, form, &mut plan).await?;
    }
    Ok(plan)
}

/// Every inline on `parent_model` whose child table this admin shows.
async fn inline_targets(
    state: &AppState,
    parts: &Parts,
    parent_model: &'static ModelSchema,
    parent_pk: &SqlValue,
) -> Result<Vec<InlineTarget>, ExecError> {
    let mut out = Vec::new();
    for inline in for_parent_table(parent_model.table) {
        let Some(child) = lookup_model(state, inline.child_table) else {
            continue;
        };
        if child.field_by_column(inline.fk_column).is_none() {
            continue;
        }
        let scope = ParentScope(vec![(inline.fk_column, parent_pk.clone())]);
        let display = resolve_render_fields(child, inline);
        out.extend(InlineTarget::new(
            parent_model.table,
            child,
            &display,
            inline.readonly_fields,
            inline.max_num,
            scope,
            state,
            parts,
        ));
    }

    let generic = generic_for_parent_table(parent_model.table);
    if generic.is_empty() {
        return Ok(out);
    }
    let Some(ct_id) = resolve_ct_id_for_schema(&state.pool, parent_model).await? else {
        return Ok(out);
    };
    let parent_pk_i64 = match parent_pk {
        SqlValue::I64(v) => *v,
        SqlValue::I32(v) => i64::from(*v),
        SqlValue::I16(v) => i64::from(*v),
        _ => return Ok(out),
    };
    for inline in generic {
        let Some(child) = lookup_model(state, inline.child_table) else {
            continue;
        };
        if child.field_by_column(inline.ct_column).is_none()
            || child.field_by_column(inline.pk_column).is_none()
        {
            continue;
        }
        let scope = ParentScope(vec![
            (inline.ct_column, SqlValue::I64(ct_id)),
            (inline.pk_column, SqlValue::I64(parent_pk_i64)),
        ]);
        let display = resolve_render_fields_generic(child, inline);
        out.extend(InlineTarget::new(
            parent_model.table,
            child,
            &display,
            inline.readonly_fields,
            inline.max_num,
            scope,
            state,
            parts,
        ));
    }
    Ok(out)
}

async fn plan_target(
    state: &AppState,
    parts: &Parts,
    target: &InlineTarget,
    form: &HashMap<String, String>,
    plan: &mut InlinePlan,
) -> Result<(), InlinePlanError> {
    let table = target.child.table;
    let prefix = target.prefix.as_str();
    // Messages name the model, as the parent's do.
    let name = target.child.name;
    // No management form: the panel was not rendered, nothing to do.
    let total_forms = match crate::forms::formset::total_forms(form, prefix) {
        Ok(n) => n,
        Err(crate::forms::formset::FormSetError::MissingTotalForms(_)) => return Ok(()),
        Err(e) => return Err(InlinePlanError::BadFormset(e)),
    };
    let refused = |action: &'static str| AdminError::Forbidden {
        table: table.to_owned(),
        action,
    };
    let read_only = || AdminError::ReadOnly {
        table: table.to_owned(),
    };
    // Never skipped quietly: the user must see the row is not saved (#2339).
    let bad_row = |idx: usize, e: crate::forms::FormError| {
        InlinePlanError::Rejected(format!("{name} row {}: {e}", idx + 1))
    };

    // Slots past INITIAL_FORMS are new rows, so a typed natural PK
    // inserts rather than updates (#1717).
    let initial = crate::forms::formset::initial_forms(form, prefix);
    let natural_pk = (!target.pk.auto).then_some(target.pk);
    let cfg = admin_config_or_default(target.child);
    let mut out = TargetPlan {
        child: target.child,
        pk: target.pk,
        existing: Vec::new(),
        inserts: Vec::new(),
        cap: None,
    };
    let is_new = |idx: usize, raw_pk: &str| slot_is_new(initial, idx, raw_pk);
    let rows = crate::forms::formset::row_payloads(form, prefix, total_forms);
    // Every existing row's stored state in one read, not one per row.
    let existing_raw: Vec<String> = rows
        .iter()
        .enumerate()
        .filter_map(|(idx, row)| {
            let raw_pk = row.get(target.pk.name)?;
            (!is_new(idx, raw_pk)).then(|| raw_pk.clone())
        })
        .collect();
    // Each key as the row stores it (a SQLite UUID may be TEXT).
    let stored: HashMap<&str, SqlValue> = super::helpers::parse_keys(
        state,
        target.child,
        target.pk,
        existing_raw.iter().map(String::as_str),
    )
    .await?
    .into_iter()
    .collect();
    let own = target
        .fetch_own_many(&state.pool, parts, stored.values().cloned().collect())
        .await?;
    // Distinct PKs: a repeated DELETE slot removes one row (#1717).
    let mut deleted: HashSet<String> = HashSet::new();
    // FK links to check, in row order: one read per target after the loop.
    let mut links: Vec<PendingLink> = Vec::new();
    let plan_rows = || -> Result<(), InlinePlanError> {
        for (idx, row) in rows.into_iter().enumerate() {
            let raw_pk = row.get(target.pk.name).cloned().unwrap_or_default();
            let delete_flag = row
                .get("DELETE")
                .is_some_and(|s| s == "on" || s == "true" || s == "1");

            if is_new(idx, &raw_pk) {
                // Blank extra rows stay blank.
                let has_content = target
                    .writable
                    .iter()
                    .chain(&natural_pk)
                    .any(|f| row.get(f.name).is_some_and(|s| !s.trim().is_empty()));
                if !has_content || delete_flag {
                    continue;
                }
                if !state.can_add(table) {
                    return Err(read_only().into());
                }
                if !crate::admin::object_permissions::is_allowed(table, "add", parts, None) {
                    return Err(refused("add").into());
                }
                let mut values = target.values(&row, false).map_err(|e| bad_row(idx, e))?;
                #[cfg(feature = "tenancy")]
                let claims = crate::tenancy::org_host::AdminClaims::of(table, &mut values, None)
                    .map_err(|e| {
                        InlinePlanError::Rejected(format!("{name} row {}: {e}", idx + 1))
                    })?;
                if let Some(pk) = natural_pk {
                    let value =
                        crate::forms::parse_pk_string(pk, &raw_pk).map_err(|e| bad_row(idx, e))?;
                    values.push((pk.column, value));
                }
                super::object_permissions::default_locked_flags(
                    target.child,
                    &target.add_locked,
                    &mut values,
                );
                links.push(PendingLink {
                    row: (idx + 1).to_string(),
                    values: values.clone(),
                    stored: None,
                });
                let (mut columns, mut sql_values): (Vec<&'static str>, Vec<SqlValue>) =
                    values.into_iter().unzip();
                for (column, value) in &target.scope.0 {
                    columns.push(column);
                    sql_values.push(value.clone());
                }
                // Schema-driven INSERT: nothing else supplies these (#1464).
                crate::forms::stamp_auto_timestamps(target.child, &mut columns, &mut sql_values);
                // MySQL cannot read back a DB-default UUID key; the scope re-read needs it.
                if target.pk.auto && !columns.contains(&target.pk.column) {
                    if let crate::core::FieldType::Uuid = target.pk.ty {
                        columns.push(target.pk.column);
                        sql_values.push(SqlValue::Uuid(uuid::Uuid::now_v7()));
                    }
                }
                let audit = {
                    let written: Vec<(&'static str, SqlValue)> = columns
                        .iter()
                        .copied()
                        .zip(sql_values.iter().cloned())
                        .collect();
                    // The pins are not form input; the snapshot still names the parent.
                    let mut row = row.clone();
                    for (column, value) in &target.scope.0 {
                        if let Some(f) = target.child.field_by_column(column) {
                            row.insert(f.name.to_owned(), value.to_display_string());
                        }
                    }
                    super::audit::audit_form(target.child, &cfg, &row, &written)
                };
                // The new PK feeds the scope re-read, audit entry and `admin_post_save`.
                let mut query = crate::core::InsertQuery::new(target.child, columns, sql_values);
                query.returning = vec![target.pk.column];
                out.inserts.push(InlineInsert {
                    query,
                    audit,
                    #[cfg(feature = "tenancy")]
                    claims,
                });
                continue;
            }

            let pk = match stored.get(raw_pk.as_str()) {
                Some(key) => key.clone(),
                None => crate::forms::parse_pk_string(target.pk, &raw_pk)
                    .map_err(|e| bad_row(idx, e))?,
            };

            if delete_flag {
                if !state.can_delete(table) {
                    return Err(read_only().into());
                }
                if !deleted.insert(pk.to_display_string()) {
                    continue;
                }
                let Some(before) = own.get(target.child, &pk, &raw_pk)? else {
                    continue;
                };
                if !crate::admin::object_permissions::is_allowed(
                    table,
                    "delete",
                    parts,
                    Some(&before),
                ) {
                    return Err(refused("delete").into());
                }
                // `row_where` holds the live-rows scope, so a stamp lands once.
                let pk_text = pk.to_display_string();
                let query = match target.child.soft_delete_column {
                    Some(col) => {
                        let stamp = SqlValue::DateTime(chrono::Utc::now());
                        let set = vec![crate::core::Assignment::new(col, stamp)];
                        InlineRemove::Soft(crate::core::UpdateQuery::new(
                            target.child,
                            set,
                            target.row_where(pk),
                        ))
                    }
                    None => InlineRemove::Hard(crate::core::DeleteQuery::new(
                        target.child,
                        target.row_where(pk),
                    )),
                };
                out.existing
                    .push(InlineWrite::Delete { query, pk: pk_text });
                continue;
            }

            let mut values = target.values(&row, true).map_err(|e| bad_row(idx, e))?;
            if values.is_empty() {
                continue;
            }
            let Some(before) = own.get(target.child, &pk, &raw_pk)? else {
                return Err(InlinePlanError::Rejected(format!(
                "{name} row {raw_pk} was deleted after this page loaded. Reload the page and try again."
            )));
            };
            #[cfg(feature = "tenancy")]
            let claims =
                crate::tenancy::org_host::AdminClaims::of(table, &mut values, Some(&before))
                    .map_err(|e| InlinePlanError::Rejected(format!("{name} row {raw_pk}: {e}")))?;
            if target.unchanged(&before, &values) {
                continue;
            }
            super::views::keep_echoed_lossy(target.child, &row, Some(&before), &mut values);
            if state.is_read_only(table) {
                return Err(read_only().into());
            }
            if !crate::admin::object_permissions::is_allowed(table, "change", parts, Some(&before))
            {
                return Err(refused("change").into());
            }
            links.push(PendingLink {
                row: raw_pk.clone(),
                values: values.clone(),
                stored: Some((pk.clone(), before)),
            });
            let audit = ChildAudit {
                pk: pk.to_display_string(),
                form: super::audit::audit_form(target.child, &cfg, &row, &values),
            };
            let set = values
                .into_iter()
                .map(|(column, value)| crate::core::Assignment::new(column, value))
                .collect();
            out.existing.push(InlineWrite::Update {
                query: crate::core::UpdateQuery::new(
                    target.child,
                    set,
                    target.row_where(pk.clone()),
                ),
                pk,
                audit,
                #[cfg(feature = "tenancy")]
                claims,
            });
        }
        Ok(())
    };
    let planned = plan_rows();
    // Rows before a refused one were checked first, so check their links first.
    let writes: Vec<super::queryset_hooks::LinkedWrite<'_>> = links
        .iter()
        .map(|l| super::queryset_hooks::LinkedWrite {
            values: &l.values,
            stored: l.stored.as_ref().map(|(pk, _)| (target.pk, pk)),
            held: l.stored.as_ref().map(|(_, row)| row),
        })
        .collect();
    if let Some((i, msg)) =
        super::views::hidden_fk_refusal(state, parts, target.child, &writes).await?
    {
        return Err(InlinePlanError::Rejected(format!(
            "{name} row {}: {msg}",
            links[i].row
        )));
    }
    planned?;
    // `max_num` caps the rows a POST that adds any may leave (#1717):
    // every live row of the parent, not only those the hooks show.
    if let Some(max) = target.max_num.filter(|_| !out.inserts.is_empty()) {
        let live = crate::soft_delete::compose_with_active(target.child, target.scope.all_where());
        let count = crate::core::CountQuery {
            model: target.child,
            where_clause: live.clone(),
            search: None,
            source: None,
        };
        let existing = usize::try_from(crate::sql::count_rows_pool(&state.pool, &count).await?)
            .unwrap_or(usize::MAX);
        let deletes = out
            .existing
            .iter()
            .filter(|w| matches!(w, InlineWrite::Delete { .. }))
            .count();
        let deletes_needed = existing
            .saturating_add(out.inserts.len())
            .saturating_sub(max);
        out.cap = Some((max, live));
        if deletes_needed > deletes {
            return Err(InlinePlanError::Rejected(format!(
                "{name} allows at most {max} rows here."
            )));
        }
    }
    plan.targets.push(out);
    Ok(())
}

/// Why [`apply_plan_tx`] failed; the whole edit rolls back (#2339).
pub(crate) enum InlineApplyError {
    /// The database refused a write to `child`.
    Write {
        child: &'static ModelSchema,
        error: ExecError,
    },
    /// A delete the inserts needed to stay within `max_num` removed nothing.
    MaxNum { child: &'static ModelSchema },
    /// A written row left the child's scope or failed its `add` hook.
    Forbidden {
        child: &'static ModelSchema,
        action: &'static str,
    },
    /// A routing value another tenant holds.
    #[cfg(feature = "tenancy")]
    Claim {
        child: &'static ModelSchema,
        error: crate::tenancy::org_host::ClaimError,
    },
}

/// Run a checked plan in the parent's `tx`; the caller rolls back on error.
/// Audit entries go in `tx` for an `audit(...)` child, else to the caller for after the commit.
pub(crate) async fn apply_plan_tx(
    tx: &mut crate::audit::PoolBoundTx<'_>,
    parts: &Parts,
    plan: InlinePlan,
) -> Result<InlineApplied, InlineApplyError> {
    use super::views::Written;
    use crate::audit::{AuditOp, DiffEmit, RowDiffWrite};
    let mut after_commit = Vec::new();
    // The rows written, for `admin_post_*` after the commit.
    let mut written_rows = Vec::new();
    // Concurrent saves of this parent queue here, so each recount sees the last commit.
    if let Some(first) = plan.targets.iter().find(|t| t.cap.is_some()) {
        let (model, pk) = plan.parent;
        if let Some(pk_field) = model.primary_key() {
            let parent = SelectQuery {
                lock_mode: Some(crate::core::LockMode {
                    silent_on_sqlite: true,
                    ..crate::core::LockMode::default()
                }),
                ..SelectQuery::by_pk(model, pk_field.column, pk)
            };
            crate::sql::select_one_row_as_json_tx(tx, &parent, &[pk_field])
                .await
                .map_err(|error| InlineApplyError::Write {
                    child: first.child,
                    error,
                })?;
        }
    }
    for target in plan.targets {
        let child = target.child;
        let table = child.table;
        let refused = |error| InlineApplyError::Write { child, error };
        let fields: Vec<&'static FieldSchema> = child.scalar_fields().collect();
        // Locked in `tx`, so an entry records the row the write changes.
        let locked = |where_clause: &WhereExpr| SelectQuery {
            where_clause: where_clause.clone(),
            lock_mode: Some(crate::core::LockMode {
                silent_on_sqlite: true,
                ..crate::core::LockMode::default()
            }),
            ..SelectQuery::new(child)
        };
        let mut entries: Vec<crate::audit::PendingEntry> = Vec::new();
        for write in target.existing {
            match write {
                InlineWrite::Update {
                    query,
                    pk,
                    audit,
                    #[cfg(feature = "tenancy")]
                    claims,
                } => {
                    let written = crate::audit::update_one_with_row_diff_tx(
                        tx,
                        &query,
                        locked(&query.where_clause),
                        &fields,
                        |row| {
                            super::audit::admin_audit_diff_entry(child, &audit.pk, row, &audit.form)
                        },
                        DiffEmit::Deferred,
                    )
                    .await
                    .map_err(refused)?;
                    if let RowDiffWrite::Written { deferred } = written {
                        let key = pk.to_display_string();
                        #[cfg(feature = "tenancy")]
                        claim_written(tx, child, &claims, &pk).await?;
                        admit_written(tx, parts, child, target.pk, pk, Written::Edited).await?;
                        entries.extend(deferred);
                        written_rows.push(ChildSignal::Save {
                            table,
                            pk: key,
                            change: true,
                        });
                    }
                }
                // 0 rows: deleted or moved since the plan was checked.
                InlineWrite::Delete { query, pk } => {
                    let select = locked(query.where_clause());
                    let before = crate::sql::select_one_row_as_json_tx(tx, &select, &fields)
                        .await
                        .map_err(refused)?;
                    if query.run(tx).await.map_err(refused)? == 0 {
                        continue;
                    }
                    if let Some(row) = before {
                        entries.push(super::audit::admin_row_snapshot_entry(
                            child,
                            pk.clone(),
                            query.op(),
                            &row,
                            None,
                        ));
                    }
                    written_rows.push(ChildSignal::Delete { table, pk });
                }
            }
        }
        // Recount in `tx`, after the deletes: a delete that removed nothing
        // frees no room, and a racing save's rows count. MySQL needs a locking
        // pk read to see the latest commit; elsewhere a plain COUNT does.
        if let Some((max, live)) = &target.cap {
            let live_rows = if tx.dialect().plain_reads_see_snapshot() {
                crate::sql::select_rows_as_json_tx(tx, &locked(live), &[target.pk])
                    .await
                    .map_err(refused)?
                    .len()
            } else {
                let count = crate::core::CountQuery {
                    model: child,
                    where_clause: live.clone(),
                    search: None,
                    source: None,
                };
                let n = crate::sql::count_rows_on(&mut **tx, &count)
                    .await
                    .map_err(refused)?;
                usize::try_from(n).unwrap_or(usize::MAX)
            };
            if live_rows.saturating_add(target.inserts.len()) > *max {
                return Err(InlineApplyError::MaxNum { child });
            }
        }
        for insert in &target.inserts {
            let (pk, entry) = crate::audit::insert_one_with_entry_tx(
                tx,
                &insert.query,
                target.pk,
                |pk| {
                    let pk = pk.to_display_string();
                    super::audit::admin_audit_entry(child, &pk, AuditOp::Create, &insert.audit)
                },
                DiffEmit::Deferred,
            )
            .await
            .map_err(refused)?;
            entries.extend(entry);
            let key = pk.to_display_string();
            #[cfg(feature = "tenancy")]
            claim_written(tx, child, &insert.claims, &pk).await?;
            admit_written(tx, parts, child, target.pk, pk, Written::Created).await?;
            written_rows.push(ChildSignal::Save {
                table,
                pk: key,
                change: false,
            });
        }
        // As on the child's own page: in `tx` for an `audit(...)` child,
        // else best-effort after the commit.
        match DiffEmit::for_model(child) {
            DiffEmit::InTx if !entries.is_empty() => {
                crate::audit::emit_in_tx(tx, &entries)
                    .await
                    .map_err(refused)?;
            }
            DiffEmit::InTx => {}
            DiffEmit::Deferred => after_commit.extend(entries),
        }
    }
    Ok(InlineApplied {
        signals: written_rows,
        audit: after_commit,
    })
}

/// What a committed inline save still owes: its `admin_post_*` signals and
/// the best-effort audit entries of children without `audit(...)`.
pub(crate) struct InlineApplied {
    pub(crate) signals: Vec<ChildSignal>,
    pub(crate) audit: Vec<crate::audit::PendingEntry>,
}

/// An extra host written inline is checked as on its own page.
#[cfg(feature = "tenancy")]
async fn claim_written(
    tx: &mut crate::sql::PoolTx<'_>,
    child: &'static ModelSchema,
    claims: &crate::tenancy::org_host::AdminClaims,
    pk: &SqlValue,
) -> Result<(), InlineApplyError> {
    claims
        .after_write(tx, pk)
        .await
        .map_err(|error| InlineApplyError::Claim { child, error })
}

/// The plan only saw the row before the write; re-check what was written.
async fn admit_written(
    tx: &mut crate::sql::PoolTx<'_>,
    parts: &Parts,
    child: &'static ModelSchema,
    pk_field: &'static FieldSchema,
    pk: SqlValue,
    written: super::views::Written,
) -> Result<(), InlineApplyError> {
    match super::views::admits_written(tx, parts, child, pk_field, pk, written).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(InlineApplyError::Forbidden {
            child,
            action: written.action(),
        }),
        Err(error) => Err(InlineApplyError::Write { child, error }),
    }
}

// ============================================================ Editable generic rendering (issue #243)

/// As [`render_form_for_parent`] but for generic inlines (#243).
/// Mirrors slice 2's editable shape — hidden child PK + DELETE box
/// on existing rows, `extra` blank rows below — except the WHERE
/// uses the parent's ContentType id + PK pair.
///
/// # Errors
/// As [`render_generic_for_parent`].
pub async fn render_form_generic_for_parent(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
) -> Result<Vec<InlineFormPanel>, ExecError> {
    render_form_generic_for_parent_in(pool, parent_model, parent_pk, None, None).await
}

/// [`render_form_generic_for_parent`] as [`render_form_for_parent_in`] scopes it.
pub(crate) async fn render_form_generic_for_parent_in(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
    req: Option<(&AppState, &Parts)>,
    submitted: Option<&HashMap<String, String>>,
) -> Result<Vec<InlineFormPanel>, ExecError> {
    let parts = req.map(|(_, p)| p);
    let registrations = generic_for_parent_table(parent_model.table);
    if registrations.is_empty() {
        return Ok(Vec::new());
    }
    let Some(ct_id) = resolve_ct_id_for_schema(pool, parent_model).await? else {
        return Ok(Vec::new());
    };
    let parent_pk_i64 = match &parent_pk {
        SqlValue::I64(v) => *v,
        SqlValue::I32(v) => i64::from(*v),
        SqlValue::I16(v) => i64::from(*v),
        _ => return Ok(Vec::new()),
    };

    let mut panels = Vec::with_capacity(registrations.len());
    for inline in registrations {
        let Some(child_model) = find_model_by_table(inline.child_table) else {
            continue;
        };
        if child_model.field_by_column(inline.ct_column).is_none()
            || child_model.field_by_column(inline.pk_column).is_none()
        {
            continue;
        }

        let display_fields = resolve_render_fields_generic(child_model, inline);
        let pk_field = child_model.primary_key();
        let order_pk: Vec<OrderItem> = pk_field
            .map(|pk| OrderItem::Column {
                column: pk.column,
                desc: false,
                nulls: NullsOrder::Default,
            })
            .into_iter()
            .collect();

        // #562 — composite AND (ct + pk); struct-update over ::new.
        let (cap, extra) = row_window(inline.extra);
        let (mut rows, fetched) = visible_child_rows(
            pool,
            SelectQuery {
                where_clause: WhereExpr::And(vec![
                    WhereExpr::Predicate(Filter {
                        column: inline.ct_column,
                        op: Op::Eq,
                        value: SqlValue::I64(ct_id),
                    }),
                    WhereExpr::Predicate(Filter {
                        column: inline.pk_column,
                        op: Op::Eq,
                        value: SqlValue::I64(parent_pk_i64),
                    }),
                ]),
                order_by: order_pk,
                limit: Some(cap as i64 + 1),
                ..SelectQuery::new(child_model)
            },
            parts,
        )
        .await?;
        let name_of = |column: &'static str| {
            child_model
                .field_by_column(column)
                .map_or(column, |f| f.name)
        };
        let more_rows_filter = cut_rows(
            &mut rows,
            fetched,
            cap,
            &[
                (name_of(inline.ct_column), ct_id.to_string()),
                (name_of(inline.pk_column), parent_pk_i64.to_string()),
            ],
        );

        let pins = [inline.ct_column, inline.pk_column];
        let prefix = formset_prefix(parent_model.table, child_model.table, &pins);
        let initial_forms = rows.len();
        let child_cfg = admin_config_or_default(child_model);
        let locked = inline_locks(
            child_model,
            inline.readonly_fields,
            &pins,
            req.map(|(s, _)| s),
        );
        let (total_forms, rendered_rows) = form_panel_rows(
            &child_cfg,
            &display_fields,
            pk_field,
            (&prefix, &locked),
            &rows,
            extra,
            submitted,
        );

        let mut field_labels: Vec<String> =
            display_fields.iter().map(|f| f.name.to_owned()).collect();
        // Append a Delete column only when there's something to delete.
        if initial_forms > 0 {
            field_labels.push("Delete".to_owned());
        }

        let label = if inline.label.is_empty() {
            child_model.name.to_owned()
        } else {
            inline.label.to_owned()
        };

        panels.push(InlineFormPanel {
            label,
            child_table: child_model.table.to_owned(),
            kind: match inline.kind {
                InlineKind::Tabular => "tabular".to_owned(),
                InlineKind::Stacked => "stacked".to_owned(),
            },
            prefix,
            total_forms,
            initial_forms,
            max_num: inline.max_num,
            field_labels,
            rows: rendered_rows,
            more_rows_filter,
        });
    }
    Ok(panels)
}

// Pre-rendered cells go into the template with `| safe`, so they are escaped here.
use crate::text::html_escape;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(crate::Model, Debug)]
    #[rustango(table = "inl_ct_parent", app = "inl_ct")]
    #[allow(dead_code)]
    pub struct CtParent {
        #[rustango(primary_key)]
        pub id: i64,
    }

    /// A generic inline resolves its parent's content type from the cache.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn parent_content_type_lookup_is_cached() {
        use crate::core::Model as _;
        let pool = Pool::Sqlite(
            crate::sql::sqlx::SqlitePool::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        crate::contenttypes::ensure_seeded(&pool).await.unwrap();
        let first = resolve_ct_id_for_schema(&pool, CtParent::SCHEMA)
            .await
            .unwrap();
        assert!(first.is_some());
        let (again, queries) = crate::test_assertions::QueryCounter::scope(async {
            let id = resolve_ct_id_for_schema(&pool, CtParent::SCHEMA)
                .await
                .unwrap();
            (id, crate::test_assertions::QueryCounter::current())
        })
        .await;
        assert_eq!((again, queries), (first, 0));
    }

    #[test]
    fn cut_rows_links_the_rest_only_past_the_cap() {
        let mut rows = vec![serde_json::json!({}); 3];
        let filter = [
            ("content_type", "4".to_owned()),
            ("object_id", "7".to_owned()),
        ];
        assert_eq!(cut_rows(&mut rows, 3, 3, &filter), None);
        assert_eq!(
            cut_rows(&mut rows, 3, 2, &filter).as_deref(),
            Some("content_type=4&object_id=7")
        );
        assert_eq!(rows.len(), 2);
        // The view hook dropped a row of a full fetch: still more to see.
        assert!(cut_rows(&mut rows, 3, 2, &filter).is_some());
        assert_eq!(row_window(5000), (0, crate::forms::formset::MAX_FORMS));
    }

    #[test]
    fn html_escape_quotes_and_brackets() {
        assert_eq!(
            html_escape("<a href='x'>&"),
            "&lt;a href=&#x27;x&#x27;&gt;&amp;"
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

    /// A read-only secret shows only whether it is set, never its value.
    #[test]
    fn a_read_only_secret_is_not_echoed() {
        let f = FieldSchema {
            name: "password_hash",
            column: "password_hash",
            ty: crate::core::FieldType::String,
            nullable: false,
            primary_key: false,
            auto: false,
            auto_now: false,
            unique: false,
            max_length: None,
            min: None,
            max: None,
            default: None,
            relation: None,
            generated_as: Some("'x'"),
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
            cast: None,
            secret: None,
        };
        let cfg = crate::core::AdminConfig::DEFAULT;
        let html = render_prefixed_input(&cfg, &f, "s3cret", ("c", 0, WriteKind::Update), false);
        assert!(!html.contains("s3cret"), "{html}");
        assert!(html.contains("<em>set</em>"), "{html}");
    }

    /// A posted row outside the capped window may still exist: the
    /// message does not call it deleted.
    #[test]
    fn a_posted_row_outside_the_window_is_not_called_deleted() {
        let form: HashMap<String, String> = [
            ("c-TOTAL_FORMS", "1"),
            ("c-INITIAL_FORMS", "1"),
            ("c-0-id", "1002"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let cfg = crate::core::AdminConfig::DEFAULT;
        let (_, rows) = form_panel_rows(&cfg, &[], None, ("c", &[]), &[], 0, Some(&form));
        let error = rows[0]["error"].as_str().expect("an error row");
        assert!(error.contains("1002"), "{error}");
        assert!(!error.contains("was deleted"), "{error}");
    }

    #[test]
    fn stringify_pk_supports_numeric_and_string_keys() {
        assert_eq!(stringify_pk(&serde_json::json!(7)), "7");
        assert_eq!(stringify_pk(&serde_json::json!("INV-1")), "INV-1");
        assert_eq!(stringify_pk(&serde_json::json!(null)), "");
    }
}
