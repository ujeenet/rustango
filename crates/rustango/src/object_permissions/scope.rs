//! Queryset scopes: a hook that reads the request and narrows the rows
//! the admin and ViewSets may show or touch, and the "view" hook folded
//! into one PK filter for lists.

use axum::http::request::Parts;

use crate::core::{Filter, ModelSchema, Op, SearchClause, SelectQuery, SqlValue, WhereExpr};

/// A queryset scope. Takes the request [`Parts`] and returns extra
/// [`Filter`]s, ANDed into every read and write of its table.
///
/// A plain `fn` pointer, not `Arc<dyn Fn>`, so the registration fits
/// in inventory's const storage.
pub type QuerySetScopeFn = fn(&Parts) -> Vec<Filter>;

/// One scope registration, collected by inventory via
/// [`crate::register_queryset_scope!`].
pub struct QuerySetScope {
    /// Model table. Must equal `ModelSchema::table`.
    pub table: &'static str,
    /// The callable.
    pub hook: QuerySetScopeFn,
}

inventory::collect!(QuerySetScope);

/// Every scope registered for `table`, in registration order.
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static QuerySetScope> {
    inventory::iter::<QuerySetScope>
        .into_iter()
        .filter(|h| h.table == table)
        .collect()
}

/// The rows a "view" hook admits, as a filter SQL can apply.
pub(crate) enum ViewHookRows {
    /// No hook, or it denies nothing.
    All,
    /// The admitted rows, by PK.
    Only(Filter),
    /// More rows than the scan cap: totals must be withheld.
    TooMany,
}

/// Run `hooks`' "view" hook over every row of `model` matching `where_clause`
/// and `search`, folded into one PK filter: the shorter of `IN` admitted or
/// `NOT IN` denied. The hook takes the whole row, so all scalar columns are read.
pub(crate) async fn view_hook_rows(
    pool: &crate::sql::Pool,
    (parts, hooks): (&Parts, super::Hooks),
    model: &'static ModelSchema,
    (where_clause, search): (WhereExpr, Option<SearchClause>),
    cap: usize,
) -> Result<ViewHookRows, crate::sql::ExecError> {
    if !hooks.any(model.table, "view") {
        return Ok(ViewHookRows::All);
    }
    let Some(pk_field) = model.primary_key() else {
        return Ok(ViewHookRows::TooMany);
    };
    let fields: Vec<_> = model.scalar_fields().collect();
    let rows = crate::sql::select_rows_as_json(
        pool,
        &SelectQuery {
            where_clause,
            search,
            limit: Some(i64::try_from(cap).unwrap_or(i64::MAX).saturating_add(1)),
            ..SelectQuery::new(model)
        },
        &fields,
    )
    .await?;
    if rows.len() > cap {
        warn_scan_cap(model.table, cap);
        return Ok(ViewHookRows::TooMany);
    }
    let (mut admitted, mut denied) = (Vec::new(), Vec::new());
    for row in &rows {
        let Some(pk) = row_pk(row, pk_field) else {
            return Ok(ViewHookRows::TooMany);
        };
        if hooks.allows(model.table, "view", parts, Some(row)) {
            admitted.push(pk);
        } else {
            denied.push(pk);
        }
    }
    let filter = |op, value| Filter {
        column: pk_field.column,
        op,
        value,
    };
    Ok(if denied.is_empty() {
        ViewHookRows::All
    } else if admitted.is_empty() {
        // A PK is never NULL: no row.
        ViewHookRows::Only(filter(Op::IsNull, SqlValue::Bool(true)))
    } else if denied.len() <= admitted.len() {
        ViewHookRows::Only(filter(Op::NotIn, SqlValue::List(denied)))
    } else {
        ViewHookRows::Only(filter(Op::In, SqlValue::List(admitted)))
    })
}

/// A JSON row's primary key as a bind value.
pub(crate) fn row_pk(
    row: &serde_json::Value,
    pk_field: &'static crate::core::FieldSchema,
) -> Option<SqlValue> {
    crate::admin::render::read_value_as_string_json(row, pk_field)
        .and_then(|raw| crate::forms::parse_pk_string(pk_field, &raw).ok())
}

/// Log once per table that the scan cap hid its totals.
pub(crate) fn warn_scan_cap(table: &'static str, cap: usize) {
    static WARNED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !warned.contains(&table) {
        warned.push(table);
        tracing::warn!(
            target: "rustango::object_permissions",
            table,
            cap,
            "the \"view\" hook matches more rows than the scan cap: totals are hidden. \
             Use register_queryset_scope! or raise the scan cap"
        );
    }
}

/// Register a queryset scope for one model. The hook must be a plain
/// `fn`, or a closure with no captures, that coerces to [`QuerySetScopeFn`].
///
/// ```ignore
/// fn only_published(_parts: &axum::http::request::Parts) -> Vec<rustango::core::Filter> {
///     vec![rustango::core::Filter::new(
///         "is_published",
///         rustango::core::Op::Eq,
///         rustango::core::SqlValue::Bool(true),
///     )]
/// }
/// rustango::register_queryset_scope!("blog_post", only_published);
/// ```
#[macro_export]
macro_rules! register_queryset_scope {
    ($table:expr, $hook:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::object_permissions::QuerySetScope {
                table: $table,
                hook: {
                    const _HOOK: $crate::object_permissions::QuerySetScopeFn = $hook;
                    _HOOK
                },
            }
        }
    };
}

/// The old name of [`register_queryset_scope!`].
#[deprecated(since = "0.62.0", note = "use `register_queryset_scope!`")]
#[macro_export]
macro_rules! register_admin_queryset {
    ($table:expr, $hook:expr $(,)?) => {
        $crate::register_queryset_scope!($table, $hook);
    };
}
