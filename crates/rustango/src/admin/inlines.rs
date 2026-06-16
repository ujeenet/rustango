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

// ============================================================ InlineAdminGeneric (issue #242)

/// One **generic** inline-admin registration. Mirrors [`InlineAdmin`]
/// but keys the relation on a `(ct_column, pk_column)` pair instead
/// of a single FK column — Django's `GenericTabularInline` /
/// `GenericStackedInline` shape.
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
    /// Field names rendered as plain text even in edit mode.
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

        // #562 — by_pk constructor + struct-update for order_by and
        // limit=None.
        let rows = select_rows_as_json(
            pool,
            &SelectQuery {
                order_by: order_pk,
                limit: None,
                ..SelectQuery::by_pk(child_model, inline.fk_column, parent_pk.clone())
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
                            "label": f.display_label(),
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

/// As [`render_for_parent`] but for generic inlines registered via
/// [`register_admin_inline_generic!`]. Each panel lists every child row
/// whose `(content_type_id, object_pk)` matches the parent.
///
/// Resolves the parent's `ContentType` via the inventory registry +
/// `ContentType::by_natural_key` (cache-hot after the first call). The
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

        // #562 — composite AND (ct + pk); struct-update over ::new.
        let rows = select_rows_as_json(
            pool,
            &SelectQuery {
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
                ..SelectQuery::new(child_model)
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
                            "label": f.display_label(),
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
    let entry = inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == schema.table);
    let Some(entry) = entry else {
        return Ok(None);
    };
    let app = entry.resolved_app_label().unwrap_or("project");
    let name = schema.name.to_ascii_lowercase();
    let ct = crate::contenttypes::ContentType::by_natural_key(pool, app, &name).await?;
    Ok(ct.and_then(|c| c.id.get().copied()))
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

// ============================================================ Editable rendering (slice 2)

/// One editable inline panel for the `form.html` template. Mirrors
/// [`InlinePanel`] but each cell carries pre-rendered `<input>`
/// HTML instead of a static value, plus the FormSet management-form
/// fields and Django's `<prefix>-N-<field>` naming.
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
    /// (Django's "no cap" sentinel).
    pub max_num: Option<usize>,
    /// Column headers, in render order. Padded with a final
    /// `"Delete"` column when at least one existing row is present
    /// (so the delete-checkbox column lines up with the others).
    pub field_labels: Vec<String>,
    /// One entry per form row. Each row carries pre-rendered
    /// `{ label, input_html }` cells, plus `pk` (empty for new rows)
    /// and `delete_input_html` (empty for new rows).
    pub rows: Vec<serde_json::Value>,
}

/// As [`render_for_parent`] but produces an editable [`InlineFormPanel`]
/// — N existing-row inputs prefilled, then `extra` blank rows below.
/// Each existing row carries a hidden `<prefix>-N-<pk>` input + a
/// `<prefix>-N-DELETE` checkbox so the POST handler can identify which
/// rows to UPDATE vs DELETE.
///
/// Pair with [`apply_post`] to round-trip a submitted edit page.
///
/// # Errors
/// As [`render_for_parent`].
pub async fn render_form_for_parent(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
) -> Result<Vec<InlineFormPanel>, ExecError> {
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

        // #562 — by_pk constructor + struct-update for order_by and
        // limit=None.
        let rows = select_rows_as_json(
            pool,
            &SelectQuery {
                order_by: order_pk,
                limit: None,
                ..SelectQuery::by_pk(child_model, inline.fk_column, parent_pk.clone())
            },
            &select_fields,
        )
        .await?;

        let prefix = child_model.table.to_owned();
        let initial_forms = rows.len();
        let total_forms = initial_forms + inline.extra;
        let pk_column = pk_field.map(|p| p.column).unwrap_or("id");

        let mut field_labels: Vec<String> =
            display_fields.iter().map(|f| f.name.to_owned()).collect();
        // Append a Delete column only when there's something to delete.
        if initial_forms > 0 {
            field_labels.push("Delete".to_owned());
        }

        let mut rendered_rows: Vec<serde_json::Value> = Vec::with_capacity(total_forms);
        for (idx, row) in rows.iter().enumerate() {
            let pk_text = row.get(pk_column).map(stringify_pk).unwrap_or_default();
            let cells: Vec<serde_json::Value> = display_fields
                .iter()
                .map(|f| {
                    // Route through the same formatter the main change form
                    // uses (#datetime-local fix) so DateTime/Json/Array values
                    // are normalised to what each `<input>` accepts — a bare
                    // `value_as_form_string` clone left e.g. a `DateTime`'s
                    // `+00:00` offset in place, which `datetime-local` rejects.
                    let raw_str = crate::admin::render::render_value_for_input_json(row, f);
                    let input_html = render_prefixed_input(f, &raw_str, &prefix, idx, false);
                    serde_json::json!({
                        "label": f.display_label(),
                        "input_html": input_html,
                    })
                })
                .collect();
            // Hidden PK input keeps the row identifiable through the
            // round-trip even if the operator changes nothing else.
            let pk_field_name = pk_field.map(|p| p.name).unwrap_or("id");
            let hidden_pk = format!(
                r#"<input type="hidden" name="{p}-{i}-{n}" value="{v}">"#,
                p = html_escape(&prefix),
                i = idx,
                n = html_escape(pk_field_name),
                v = html_escape(&pk_text),
            );
            let delete_input_html = format!(
                r#"<input type="checkbox" name="{p}-{i}-DELETE" value="on">"#,
                p = html_escape(&prefix),
                i = idx,
            );
            rendered_rows.push(serde_json::json!({
                "pk": pk_text,
                "cells": cells,
                "hidden_pk": hidden_pk,
                "delete_input_html": delete_input_html,
            }));
        }
        // Extra blank rows for adding new children. No hidden PK (the
        // POST handler treats absence as "INSERT") and no DELETE box.
        for idx in initial_forms..total_forms {
            let cells: Vec<serde_json::Value> = display_fields
                .iter()
                .map(|f| {
                    let input_html = render_prefixed_input(f, "", &prefix, idx, false);
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
        });
    }
    Ok(panels)
}

/// Wrap `super::render::render_input` so the generated `name=` /
/// `id=` attributes are prefix-mangled into Django's FormSet
/// `<prefix>-<idx>-<field>` shape. We accomplish this with a
/// `str::replace` on the rendered HTML — `render_input` always emits
/// `name="<field>"` and `id="<field>"`, so the substitution is
/// uniquely targetable.
fn render_prefixed_input(
    field: &FieldSchema,
    value: &str,
    prefix: &str,
    idx: usize,
    pk_locked: bool,
) -> String {
    let base = crate::admin::render::render_input(field, value, pk_locked);
    let target_name = format!(r#"name="{}""#, field.name);
    let new_name = format!(r#"name="{prefix}-{idx}-{}""#, field.name);
    let target_id = format!(r#"id="{}""#, field.name);
    let new_id = format!(r#"id="{prefix}-{idx}-{}""#, field.name);
    base.replacen(&target_name, &new_name, 1)
        .replacen(&target_id, &new_id, 1)
}

// ============================================================ POST processing (slice 2)

/// Outcome of processing a single inline POST payload. Aggregated
/// across panels by [`apply_post`] so the caller can report counts to
/// the operator without caring about per-row mechanics.
#[derive(Debug, Default, Clone, Copy)]
pub struct InlineApplyOutcome {
    /// Existing rows successfully updated.
    pub updated: usize,
    /// Existing rows successfully deleted (DELETE checkbox was on).
    pub deleted: usize,
    /// New rows successfully inserted (extra/empty slots with content).
    pub inserted: usize,
    /// Rows that failed to validate or write — counted but skipped.
    pub failed: usize,
}

impl InlineApplyOutcome {
    fn add(&mut self, other: Self) {
        self.updated += other.updated;
        self.deleted += other.deleted;
        self.inserted += other.inserted;
        self.failed += other.failed;
    }
}

/// Process every inline FormSet payload on a parent edit POST. For
/// each row: an empty PK + any non-empty field → INSERT; a present PK
/// + DELETE checkbox → DELETE; a present PK + no DELETE → UPDATE.
///
/// `parent_pk` is the parent's primary-key value — every INSERT pins
/// the FK column to it.
///
/// Best-effort: a row that fails to validate or write increments
/// `failed` and is skipped; other rows continue. The caller decides
/// whether to surface the failure (the admin's `update_submit`
/// reports a flash message via the audit log path).
///
/// # Errors
/// Only when the registration's child model isn't in the registry —
/// per-row write errors are absorbed into `failed`.
pub async fn apply_post(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
    form: &std::collections::HashMap<String, String>,
) -> Result<InlineApplyOutcome, ExecError> {
    let registrations = for_parent_table(parent_model.table);
    let mut total = InlineApplyOutcome::default();
    for inline in registrations {
        let Some(child_model) = find_model_by_table(inline.child_table) else {
            continue;
        };
        if child_model.field_by_column(inline.fk_column).is_none() {
            continue;
        }
        let prefix = child_model.table;
        let total_forms = match crate::forms::formset::total_forms(form, prefix) {
            Ok(n) => n,
            Err(_) => {
                // No management form for this inline (operator left
                // the panel hidden, JS didn't render it, etc.). Skip
                // silently — slice 1's display path doesn't render
                // the management inputs either.
                continue;
            }
        };
        let outcome = apply_one_inline(
            pool,
            child_model,
            inline,
            prefix,
            total_forms,
            &parent_pk,
            form,
        )
        .await;
        total.add(outcome);
    }
    Ok(total)
}

async fn apply_one_inline(
    pool: &Pool,
    child_model: &'static ModelSchema,
    inline: &InlineAdmin,
    prefix: &str,
    total_forms: usize,
    parent_pk: &SqlValue,
    form: &std::collections::HashMap<String, String>,
) -> InlineApplyOutcome {
    let mut outcome = InlineApplyOutcome::default();
    let pk_field = match child_model.primary_key() {
        Some(p) => p,
        None => return outcome,
    };
    let display_fields = resolve_render_fields(child_model, inline);

    for idx in 0..total_forms {
        let row = crate::forms::formset::row_payload(form, prefix, idx);
        let raw_pk = row.get(pk_field.name).cloned().unwrap_or_default();
        let has_pk = !raw_pk.trim().is_empty();
        let delete_flag = row
            .get("DELETE")
            .map(|s| s == "on" || s == "true" || s == "1")
            .unwrap_or(false);

        // Existing row + DELETE → DELETE child row.
        if has_pk && delete_flag {
            let pk_val = match crate::forms::parse_pk_string(pk_field, &raw_pk) {
                Ok(v) => v,
                Err(_) => {
                    outcome.failed += 1;
                    continue;
                }
            };
            let q = crate::core::DeleteQuery {
                model: child_model,
                where_clause: WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_val,
                }),
            };
            match crate::sql::delete_pool(pool, &q).await {
                Ok(_) => outcome.deleted += 1,
                Err(_) => outcome.failed += 1,
            }
            continue;
        }

        // Existing row, no DELETE → UPDATE child row.
        if has_pk {
            let pk_val = match crate::forms::parse_pk_string(pk_field, &raw_pk) {
                Ok(v) => v,
                Err(_) => {
                    outcome.failed += 1;
                    continue;
                }
            };
            let assignments = match build_assignments(&display_fields, &row, Some(inline.fk_column))
            {
                Ok(a) => a,
                Err(_) => {
                    outcome.failed += 1;
                    continue;
                }
            };
            if assignments.is_empty() {
                // Nothing changed; not a failure.
                continue;
            }
            let q = crate::core::UpdateQuery {
                model: child_model,
                set: assignments,
                where_clause: WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_val,
                }),
            };
            match crate::sql::update_pool(pool, &q).await {
                Ok(_) => outcome.updated += 1,
                Err(_) => outcome.failed += 1,
            }
            continue;
        }

        // No PK → INSERT, but only when the operator typed something
        // into at least one display field. Empty extras stay empty.
        let row_nonempty = display_fields.iter().any(|f| {
            row.get(f.name)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        });
        if !row_nonempty {
            continue;
        }
        let assignments = match build_assignments(&display_fields, &row, None) {
            Ok(a) => a,
            Err(_) => {
                outcome.failed += 1;
                continue;
            }
        };
        // Pin the FK to the parent's PK so the new row attaches.
        // `build_assignments` returns `Assignment` whose `value` is an
        // `Expr::Literal(SqlValue)` — unwrap back to the SqlValue for
        // InsertQuery's positional `values: Vec<SqlValue>` shape.
        let mut columns: Vec<&'static str> = assignments.iter().map(|a| a.column).collect();
        let mut values: Vec<SqlValue> = assignments
            .into_iter()
            .map(|a| match a.value {
                crate::core::Expr::Literal(v) => v,
                _ => SqlValue::Null,
            })
            .collect();
        // Skip duplicate FK assignment if for some reason the operator
        // also submitted the FK column directly.
        if !columns.contains(&inline.fk_column) {
            columns.push(inline.fk_column);
            values.push(parent_pk.clone());
        }
        let q = crate::core::InsertQuery {
            model: child_model,
            columns,
            values,
            returning: vec![],
            on_conflict: None,
        };
        match crate::sql::insert_pool(pool, &q).await {
            Ok(_) => outcome.inserted += 1,
            Err(_) => outcome.failed += 1,
        }
    }

    outcome
}

/// Translate one row payload into `Assignment` values keyed on SQL
/// columns. `skip_column`, when set, drops that column from the
/// output — UPDATE skips the FK so a malicious POST can't reparent
/// a child row to another parent.
fn build_assignments(
    display_fields: &[&'static FieldSchema],
    row: &std::collections::HashMap<String, String>,
    skip_column: Option<&str>,
) -> Result<Vec<crate::core::Assignment>, crate::forms::FormError> {
    let mut out = Vec::with_capacity(display_fields.len());
    for f in display_fields {
        if let Some(skip) = skip_column {
            if f.column == skip {
                continue;
            }
        }
        let raw = row.get(f.name).map(String::as_str);
        // Empty strings + nullable → NULL. Empty strings + required:
        // bubble up the FormError so the panel marks this row failed.
        let value = crate::forms::parse_form_value(f, raw)?;
        out.push(crate::core::Assignment {
            column: f.column,
            value: crate::core::Expr::Literal(value),
        });
    }
    Ok(out)
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

        // #562 — composite AND (ct + pk); struct-update over ::new.
        let rows = select_rows_as_json(
            pool,
            &SelectQuery {
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
                ..SelectQuery::new(child_model)
            },
            &select_fields,
        )
        .await?;

        let prefix = child_model.table.to_owned();
        let initial_forms = rows.len();
        let total_forms = initial_forms + inline.extra;
        let pk_column = pk_field.map(|p| p.column).unwrap_or("id");

        let mut field_labels: Vec<String> =
            display_fields.iter().map(|f| f.name.to_owned()).collect();
        if initial_forms > 0 {
            field_labels.push("Delete".to_owned());
        }

        let mut rendered_rows: Vec<serde_json::Value> = Vec::with_capacity(total_forms);
        for (idx, row) in rows.iter().enumerate() {
            let pk_text = row.get(pk_column).map(stringify_pk).unwrap_or_default();
            let cells: Vec<serde_json::Value> = display_fields
                .iter()
                .map(|f| {
                    // Route through the same formatter the main change form
                    // uses (#datetime-local fix) so DateTime/Json/Array values
                    // are normalised to what each `<input>` accepts — a bare
                    // `value_as_form_string` clone left e.g. a `DateTime`'s
                    // `+00:00` offset in place, which `datetime-local` rejects.
                    let raw_str = crate::admin::render::render_value_for_input_json(row, f);
                    let input_html = render_prefixed_input(f, &raw_str, &prefix, idx, false);
                    serde_json::json!({
                        "label": f.display_label(),
                        "input_html": input_html,
                    })
                })
                .collect();
            let pk_field_name = pk_field.map(|p| p.name).unwrap_or("id");
            let hidden_pk = format!(
                r#"<input type="hidden" name="{p}-{i}-{n}" value="{v}">"#,
                p = html_escape(&prefix),
                i = idx,
                n = html_escape(pk_field_name),
                v = html_escape(&pk_text),
            );
            let delete_input_html = format!(
                r#"<input type="checkbox" name="{p}-{i}-DELETE" value="on">"#,
                p = html_escape(&prefix),
                i = idx,
            );
            rendered_rows.push(serde_json::json!({
                "pk": pk_text,
                "cells": cells,
                "hidden_pk": hidden_pk,
                "delete_input_html": delete_input_html,
            }));
        }
        for idx in initial_forms..total_forms {
            let cells: Vec<serde_json::Value> = display_fields
                .iter()
                .map(|f| {
                    let input_html = render_prefixed_input(f, "", &prefix, idx, false);
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
        });
    }
    Ok(panels)
}

/// Process every generic-inline FormSet payload on a parent edit POST.
/// Same dispatch rules as [`apply_post`] but pins BOTH
/// `ct_column = parent_ct_id` AND `pk_column = parent_pk` on INSERT
/// and skips BOTH on UPDATE (so a malicious POST can't reparent a
/// generic-inline row to a different `(content_type, target)` pair).
///
/// # Errors
/// As [`apply_post`].
pub async fn apply_post_generic(
    pool: &Pool,
    parent_model: &'static ModelSchema,
    parent_pk: SqlValue,
    form: &std::collections::HashMap<String, String>,
) -> Result<InlineApplyOutcome, ExecError> {
    let registrations = generic_for_parent_table(parent_model.table);
    if registrations.is_empty() {
        return Ok(InlineApplyOutcome::default());
    }
    let Some(ct_id) = resolve_ct_id_for_schema(pool, parent_model).await? else {
        return Ok(InlineApplyOutcome::default());
    };
    let parent_pk_i64 = match &parent_pk {
        SqlValue::I64(v) => *v,
        SqlValue::I32(v) => i64::from(*v),
        SqlValue::I16(v) => i64::from(*v),
        _ => return Ok(InlineApplyOutcome::default()),
    };

    let mut total = InlineApplyOutcome::default();
    for inline in registrations {
        let Some(child_model) = find_model_by_table(inline.child_table) else {
            continue;
        };
        if child_model.field_by_column(inline.ct_column).is_none()
            || child_model.field_by_column(inline.pk_column).is_none()
        {
            continue;
        }
        let prefix = child_model.table;
        let total_forms = match crate::forms::formset::total_forms(form, prefix) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let outcome = apply_one_inline_generic(
            pool,
            child_model,
            inline,
            prefix,
            total_forms,
            ct_id,
            parent_pk_i64,
            form,
        )
        .await;
        total.add(outcome);
    }
    Ok(total)
}

async fn apply_one_inline_generic(
    pool: &Pool,
    child_model: &'static ModelSchema,
    inline: &InlineAdminGeneric,
    prefix: &str,
    total_forms: usize,
    parent_ct_id: i64,
    parent_pk_i64: i64,
    form: &std::collections::HashMap<String, String>,
) -> InlineApplyOutcome {
    let mut outcome = InlineApplyOutcome::default();
    let pk_field = match child_model.primary_key() {
        Some(p) => p,
        None => return outcome,
    };
    let display_fields = resolve_render_fields_generic(child_model, inline);

    for idx in 0..total_forms {
        let row = crate::forms::formset::row_payload(form, prefix, idx);
        let raw_pk = row.get(pk_field.name).cloned().unwrap_or_default();
        let has_pk = !raw_pk.trim().is_empty();
        let delete_flag = row
            .get("DELETE")
            .map(|s| s == "on" || s == "true" || s == "1")
            .unwrap_or(false);

        // Existing row + DELETE → DELETE child row.
        if has_pk && delete_flag {
            let pk_val = match crate::forms::parse_pk_string(pk_field, &raw_pk) {
                Ok(v) => v,
                Err(_) => {
                    outcome.failed += 1;
                    continue;
                }
            };
            let q = crate::core::DeleteQuery {
                model: child_model,
                where_clause: WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_val,
                }),
            };
            match crate::sql::delete_pool(pool, &q).await {
                Ok(_) => outcome.deleted += 1,
                Err(_) => outcome.failed += 1,
            }
            continue;
        }

        // Existing row, no DELETE → UPDATE child row, skipping both
        // polymorphic columns so the relationship stays pinned to
        // this parent.
        if has_pk {
            let pk_val = match crate::forms::parse_pk_string(pk_field, &raw_pk) {
                Ok(v) => v,
                Err(_) => {
                    outcome.failed += 1;
                    continue;
                }
            };
            let assignments = match build_assignments_generic(
                &display_fields,
                &row,
                inline.ct_column,
                inline.pk_column,
            ) {
                Ok(a) => a,
                Err(_) => {
                    outcome.failed += 1;
                    continue;
                }
            };
            if assignments.is_empty() {
                continue;
            }
            let q = crate::core::UpdateQuery {
                model: child_model,
                set: assignments,
                where_clause: WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_val,
                }),
            };
            match crate::sql::update_pool(pool, &q).await {
                Ok(_) => outcome.updated += 1,
                Err(_) => outcome.failed += 1,
            }
            continue;
        }

        // No PK → INSERT, but only when the operator typed something.
        let row_nonempty = display_fields.iter().any(|f| {
            row.get(f.name)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        });
        if !row_nonempty {
            continue;
        }
        let assignments = match build_assignments_generic(
            &display_fields,
            &row,
            inline.ct_column,
            inline.pk_column,
        ) {
            Ok(a) => a,
            Err(_) => {
                outcome.failed += 1;
                continue;
            }
        };
        // Pin BOTH polymorphic columns to the parent's CT id + PK.
        let mut columns: Vec<&'static str> = assignments.iter().map(|a| a.column).collect();
        let mut values: Vec<SqlValue> = assignments
            .into_iter()
            .map(|a| match a.value {
                crate::core::Expr::Literal(v) => v,
                _ => SqlValue::Null,
            })
            .collect();
        if !columns.contains(&inline.ct_column) {
            columns.push(inline.ct_column);
            values.push(SqlValue::I64(parent_ct_id));
        }
        if !columns.contains(&inline.pk_column) {
            columns.push(inline.pk_column);
            values.push(SqlValue::I64(parent_pk_i64));
        }
        let q = crate::core::InsertQuery {
            model: child_model,
            columns,
            values,
            returning: vec![],
            on_conflict: None,
        };
        match crate::sql::insert_pool(pool, &q).await {
            Ok(_) => outcome.inserted += 1,
            Err(_) => outcome.failed += 1,
        }
    }

    outcome
}

/// As [`build_assignments`] but skips BOTH polymorphic columns so an
/// UPDATE / INSERT can't smuggle in a different `(content_type_id,
/// object_pk)` pair through the inline FormSet payload.
fn build_assignments_generic(
    display_fields: &[&'static FieldSchema],
    row: &std::collections::HashMap<String, String>,
    ct_column: &str,
    pk_column: &str,
) -> Result<Vec<crate::core::Assignment>, crate::forms::FormError> {
    let mut out = Vec::with_capacity(display_fields.len());
    for f in display_fields {
        if f.column == ct_column || f.column == pk_column {
            continue;
        }
        let raw = row.get(f.name).map(String::as_str);
        let value = crate::forms::parse_form_value(f, raw)?;
        out.push(crate::core::Assignment {
            column: f.column,
            value: crate::core::Expr::Literal(value),
        });
    }
    Ok(out)
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
