//! Many-to-many manager — CRUD operations on junction tables.
//!
//! Obtain an instance via the macro-generated `<name>_m2m()` method on any
//! model that declares a `#[rustango(m2m(...))]` relation.
//!
//! # Example
//!
//! ```ignore
//! // Fetch all tag IDs for a post:
//! let tag_ids = post.tags_m2m().all(&pool).await?;
//!
//! // Add a tag:
//! post.tags_m2m().add(42, &pool).await?;
//!
//! // Remove a tag:
//! post.tags_m2m().remove(42, &pool).await?;
//!
//! // Replace all tags:
//! post.tags_m2m().set(&[1, 2, 3], &pool).await?;
//!
//! // Clear all tags:
//! post.tags_m2m().clear(&pool).await?;
//!
//! // Check membership:
//! let has = post.tags_m2m().contains(42, &pool).await?;
//! ```
//!
//! ## Backend coverage (v0.43 bare-name)
//!
//! Each CRUD method now ships under a **bare name** (`all`, `add`,
//! `remove`, `set`, `clear`, `contains`) that takes a `&Pool` and
//! dispatches per-backend through [`crate::sql::Pool`]. The legacy
//! `_pool` aliases (`all_pool` etc.) stay as `#[deprecated]`
//! forwarders so existing call sites still compile — they emit one
//! warning each and will be removed in a future major version.
//!
//! The pre-#891 `&PgPool`-typed wrappers (`fn all(&self, &PgPool)`
//! etc.) have been removed; the v0.34-era source-compat window
//! lapsed when v0.35 shipped the tri-dialect Pool.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use super::error::ExecError;
use super::{FlatScalar, Pool};
use crate::core::{
    BulkInsertQuery, CountQuery, DeleteQuery, FieldSchema, FieldType, Filter, InsertQuery,
    ModelSchema, Op, QueryError, SelectQuery, SqlValue, WhereExpr,
};

/// Manages the rows in a junction table for one source instance.
///
/// Constructed by the macro-generated `<name>_m2m()` method — do not build
/// directly.
pub struct M2MManager {
    /// PK value of the source model instance.
    pub src_pk: SqlValue,
    /// SQL name of the junction table (e.g. `"post_tags"`).
    pub through: &'static str,
    /// Column in `through` that references the source model's PK.
    pub src_col: &'static str,
    /// Column in `through` that references the target model's PK.
    pub dst_col: &'static str,
}

impl M2MManager {
    /// Every destination PK linked to the source instance, decoded as the
    /// target's key type `K` (`i64`, `String`, `Uuid`, ...).
    ///
    /// # Errors
    /// Driver failures, including a `dst` column that does not decode as `K`.
    pub async fn all<K: FlatScalar>(&self, pool: &Pool) -> Result<Vec<K>, ExecError> {
        let src = self.src_key()?;
        fetch_dst(
            self.junction(),
            self.dst_col,
            vec![eq(self.src_col, src)],
            pool,
        )
        .await
    }

    /// Add `dst_id` to the junction table. No-op if already present; a
    /// duplicate fires no signal. One insert-or-ignore when the junction has
    /// a unique `(src, dst)` key; a keyless through model checks then inserts,
    /// so two concurrent adds of one link on PG/MySQL can both insert.
    ///
    /// # Errors
    /// Driver failures, or a key outside the through model's field bounds.
    pub async fn add(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let src = self.src_key()?;
        let query = InsertQuery::new(
            self.junction(),
            vec![self.src_col, self.dst_col],
            vec![src.clone(), dst.clone()],
        );
        let link = vec![eq(self.src_col, src.clone()), eq(self.dst_col, dst.clone())];
        // A skipped duplicate changed nothing, so it fires no signal (#2221).
        if !insert_link(self.link_key(), link, &query, pool).await? {
            return Ok(());
        }
        // #410 — fire m2m_changed after successful junction-row write.
        // `signals` is an optional feature, so every emission below is gated
        // (#1208) — these statements were unconditional while the module is not.
        #[cfg(feature = "signals")]
        self.signal(
            crate::signals::m2m::M2mAction::Add,
            src,
            vec![dst.clone()],
            SetDiff {
                added: vec![dst],
                removed: Vec::new(),
            },
        )
        .await;
        Ok(())
    }

    /// Remove `dst_id` from the junction table. No-op if not present.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn remove(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let src = self.src_key()?;
        let filters = vec![eq(self.src_col, src.clone()), eq(self.dst_col, dst.clone())];
        if delete(self.junction(), filters, pool).await? == 0 {
            return Ok(());
        }
        // #410 — fire m2m_changed after successful junction-row remove.
        #[cfg(feature = "signals")]
        self.signal(
            crate::signals::m2m::M2mAction::Remove,
            src,
            vec![dst.clone()],
            SetDiff {
                added: Vec::new(),
                removed: vec![dst],
            },
        )
        .await;
        Ok(())
    }

    /// Replace the full set of linked destination PKs with `ids`.
    /// Atomic: DELETE + multi-row INSERT inside one transaction so
    /// concurrent readers never see the intermediate empty state.
    /// Tri-dialect via per-backend `.begin()`.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn set<K: Clone + Into<SqlValue>>(
        &self,
        ids: &[K],
        pool: &Pool,
    ) -> Result<(), ExecError> {
        let src = self.src_key()?;
        let dsts = dst_keys(ids, self.through, self.dst_col)?;
        let rows = dsts.iter().map(|d| vec![src.clone(), d.clone()]).collect();
        let locks = self.source_locks(&src);
        if locks.is_empty() {
            tracing::warn!(
                through = self.through,
                src_col = self.src_col,
                "M2M set: no registered source model, so concurrent sets are not serialized"
            );
        }
        let Some(diff) = replace(
            locks,
            self.junction(),
            vec![eq(self.src_col, src.clone())],
            self.dst_col,
            &dsts,
            vec![self.src_col, self.dst_col],
            rows,
            pool,
        )
        .await?
        else {
            return Ok(());
        };
        // `dst_pks` is the new full set (may be empty when `set([])` was called).
        #[cfg(feature = "signals")]
        self.signal(crate::signals::m2m::M2mAction::Set, src, dsts, diff)
            .await;
        #[cfg(not(feature = "signals"))]
        let _ = diff;
        Ok(())
    }

    /// Remove all junction rows for the source instance.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn clear(&self, pool: &Pool) -> Result<(), ExecError> {
        let src = self.src_key()?;
        if delete(self.junction(), vec![eq(self.src_col, src.clone())], pool).await? == 0 {
            return Ok(());
        }
        #[cfg(feature = "signals")]
        self.signal(
            crate::signals::m2m::M2mAction::Clear,
            src,
            Vec::new(),
            SetDiff::default(),
        )
        .await;
        Ok(())
    }

    /// Return `true` if `dst_id` is linked to the source instance.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn contains(
        &self,
        dst_id: impl Into<SqlValue>,
        pool: &Pool,
    ) -> Result<bool, ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let src = self.src_key()?;
        let filters = vec![eq(self.src_col, src), eq(self.dst_col, dst)];
        exists(self.junction(), filters, pool).await
    }

    fn src_key(&self) -> Result<SqlValue, ExecError> {
        src_key(&self.src_pk, self.through)
    }

    #[cfg(feature = "signals")]
    async fn signal(
        &self,
        action: crate::signals::m2m::M2mAction,
        src: SqlValue,
        dst_pks: Vec<SqlValue>,
        diff: SetDiff,
    ) {
        let ctx = crate::signals::m2m::M2mChangedContext::new(
            action,
            self.through,
            self.src_col,
            self.dst_col,
            src,
            dst_pks,
        )
        .with_diff(diff.added, diff.removed);
        crate::signals::m2m::send_m2m_changed(ctx).await;
    }

    /// The manager does not name its source (a new pub field would break
    /// literals), so lock every registered model declaring this junction end.
    fn source_locks(&self, src: &SqlValue) -> Vec<SelectQuery> {
        inventory::iter::<crate::core::ModelEntry>
            .into_iter()
            .map(|e| e.schema)
            .filter(|s| {
                s.m2m
                    .iter()
                    .any(|r| r.through == self.through && r.src_col == self.src_col)
            })
            .filter_map(|s| source_lock(s, src))
            .collect()
    }

    fn junction(&self) -> &'static ModelSchema {
        junction(self.through, &[self.src_col, self.dst_col])
    }

    /// Whether the junction itself rejects a duplicate `(src, dst)` link.
    fn link_key(&self) -> LinkKey {
        let auto = inventory::iter::<crate::core::ModelEntry>
            .into_iter()
            .flat_map(|e| e.schema.m2m.iter())
            .any(|r| r.auto_create && r.through == self.through);
        let ends = [self.src_col, self.dst_col];
        let keyed = |s: &ModelSchema| {
            s.fields
                .iter()
                .any(|f| (f.primary_key || f.unique) && ends.contains(&f.column))
                || s.indexes.iter().any(|i| {
                    i.unique
                        && i.where_clause.is_none()
                        && i.columns.iter().all(|c| ends.contains(c))
                })
        };
        let registered = crate::core::ModelEntry::for_table(self.through).map(|e| e.schema);
        if auto || registered.is_some_and(keyed) {
            LinkKey::Unique
        } else {
            LinkKey::Unkeyed
        }
    }
}

/// How `add` keeps a link from being stored twice.
#[derive(Clone, Copy)]
enum LinkKey {
    /// A unique key over `(src, dst)` or a subset: insert-or-ignore alone.
    Unique,
    /// No such key: check for the link, then insert.
    Unkeyed,
}

/// The junction as a schema the emitters compile against (#2136): the
/// registered through model, else one naming just the manager's columns.
pub(crate) fn junction(through: &'static str, cols: &[&'static str]) -> &'static ModelSchema {
    let cols: Vec<_> = cols
        .iter()
        .map(|c| (*c, auto_column_type(through, c).unwrap_or(FieldType::I64)))
        .collect();
    typed_junction(through, &cols)
}

/// The type of a junction column referencing `target`'s pk.
pub(crate) fn end_type(target: &ModelSchema) -> Option<FieldType> {
    target.primary_key().map(|pk| junction_column_type(pk.ty))
}

/// A junction column's type for a pk of type `pk`: integers widen to BIGINT.
/// The one rule for the runtime and the migration writer, whose MySQL DDL
/// keeps the pk's width ([`crate::sql::Dialect::fk_needs_same_int_width`]).
pub(crate) fn junction_column_type(pk: FieldType) -> FieldType {
    match pk {
        FieldType::I16 | FieldType::I32 => FieldType::I64,
        ty => ty,
    }
}

/// `column`'s type in an auto-created junction `through`: its end's pk type,
/// `I64` when that model is not registered. `None` for no such column.
fn auto_column_type(through: &str, column: &str) -> Option<FieldType> {
    inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .flat_map(|e| e.schema.m2m.iter().map(move |r| (e.schema, r)))
        .filter(|(_, r)| r.auto_create && r.through == through)
        .find_map(|(src, r)| {
            let end = if r.dst_col == column {
                r.to
            } else if r.src_col == column {
                src.table
            } else {
                return None;
            };
            let target = crate::core::ModelEntry::for_table(end).map(|e| e.schema);
            Some(target.and_then(end_type).unwrap_or(FieldType::I64))
        })
}

/// [`junction`] with each column's real type, for callers that decode it.
pub(crate) fn typed_junction(
    through: &'static str,
    cols: &[(&'static str, FieldType)],
) -> &'static ModelSchema {
    type Key = (&'static str, Vec<(&'static str, FieldType)>);
    // Keys are `&'static` names from `m2m(...)` declarations, so this stays small.
    static CACHE: OnceLock<Mutex<HashMap<Key, &'static ModelSchema>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache.entry((through, cols.to_vec())).or_insert_with(|| {
        if let Some(entry) = crate::core::ModelEntry::for_table(through) {
            return entry.schema;
        }
        let fields: Vec<FieldSchema> = cols
            .iter()
            .map(|(c, ty)| FieldSchema::new(c, c, *ty))
            .collect();
        let mut schema = ModelSchema::new(through, through);
        schema.fields = Box::leak(fields.into_boxed_slice());
        Box::leak(Box::new(schema))
    })
}

fn eq(column: &'static str, value: SqlValue) -> Filter {
    Filter {
        column,
        op: Op::Eq,
        value,
    }
}

/// The `dst` column of the matching junction rows, decoded as `K`.
async fn fetch_dst<K: FlatScalar>(
    junction: &'static ModelSchema,
    dst_col: &'static str,
    filters: Vec<Filter>,
    pool: &Pool,
) -> Result<Vec<K>, ExecError> {
    let mut query = SelectQuery::new(junction).where_clause(WhereExpr::and_predicates(filters));
    query.projection = Some(vec![dst_col]);
    super::executor::fetch_values_flat(pool, &query).await
}

async fn exists(
    junction: &'static ModelSchema,
    filters: Vec<Filter>,
    pool: &Pool,
) -> Result<bool, ExecError> {
    let select = SelectQuery::new(junction).where_clause(WhereExpr::and_predicates(filters));
    let n = super::executor::count_rows_pool(pool, &CountQuery::from_select(select)).await?;
    Ok(n > 0)
}

async fn delete(
    junction: &'static ModelSchema,
    filters: Vec<Filter>,
    pool: &Pool,
) -> Result<u64, ExecError> {
    let query = DeleteQuery::new(junction, WhereExpr::and_predicates(filters));
    super::executor::delete_pool(pool, &query).await
}

/// `SELECT pk … FOR NO KEY UPDATE` on the source row. Without it, two PG
/// `set`s under READ COMMITTED both delete the old rows and keep both inserts.
pub(crate) fn source_lock(source: &'static ModelSchema, pk: &SqlValue) -> Option<SelectQuery> {
    let col = source.pk_column()?;
    let mut q =
        SelectQuery::new(source).where_clause(WhereExpr::and_predicates(vec![eq(col, pk.clone())]));
    q.projection = Some(vec![col]);
    q.lock_mode = Some(crate::core::LockMode {
        no_key: true,
        silent_on_sqlite: true,
        ..crate::core::LockMode::default()
    });
    Some(q)
}

/// Insert `query` unless the link is present. `true` only when a row went
/// in. No source lock: a caller holding that row in its own transaction
/// would wait on itself.
async fn insert_link(
    key: LinkKey,
    link: Vec<Filter>,
    query: &InsertQuery,
    pool: &Pool,
) -> Result<bool, ExecError> {
    if let LinkKey::Unique = key {
        return super::executor::insert_or_ignore(pool, query).await;
    }
    query.validate()?;
    // SQLite serializes the check and insert at BEGIN; PG/MySQL can race.
    let mut tx = crate::sql::write_transaction(pool).await?;
    let present = SelectQuery::new(query.model).where_clause(WhereExpr::and_predicates(link));
    if super::executor::count_rows_on(&mut tx, &CountQuery::from_select(present)).await? > 0 {
        return Ok(false);
    }
    // A unique key the check did not cover may still skip the row.
    let inserted = super::executor::insert_or_ignore_tx(&mut tx, query).await?;
    tx.commit().await.map_err(ExecError::Driver)?;
    Ok(inserted)
}

/// The destination keys a change linked and unlinked.
#[derive(Default)]
struct SetDiff {
    added: Vec<SqlValue>,
    removed: Vec<SqlValue>,
}

/// Delete the rows `owner` matches and insert `rows` in one transaction, so
/// readers never see the empty state in between. `Some` when the set of
/// `dst_col` keys changed; `dsts` is the new set, deduplicated.
#[allow(clippy::too_many_arguments)]
async fn replace(
    locks: Vec<SelectQuery>,
    junction: &'static ModelSchema,
    owner: Vec<Filter>,
    dst_col: &'static str,
    dsts: &[SqlValue],
    columns: Vec<&'static str>,
    rows: Vec<Vec<SqlValue>>,
    pool: &Pool,
) -> Result<Option<SetDiff>, ExecError> {
    // A bad key fails before the DELETE runs, not after.
    let ins = BulkInsertQuery::new(junction, columns, rows);
    ins.validate()?;
    // SQLite has no row lock: take its write lock at BEGIN instead.
    let mut tx = crate::sql::write_transaction(pool).await?;
    for lock in locks {
        let stmt = pool.dialect().compile_select(&lock)?;
        super::executor::raw_execute_tx(&mut tx, &stmt.sql, stmt.params).await?;
    }
    let mut old = SelectQuery::new(junction).where_clause(WhereExpr::and_predicates(owner.clone()));
    old.projection = Some(vec![dst_col]);
    // An unregistered junction's column type is unknown: read it as the new keys' type.
    let old = match dst_column_type(junction.table, dst_col) {
        Some(ty) => keys_on(&mut tx, &old, Some(ty)).await?,
        None => {
            let ty = dsts.first().and_then(SqlValue::field_type);
            match keys_on(&mut tx, &old, ty).await {
                Err(ExecError::Driver(sqlx::Error::ColumnDecode { .. })) => None,
                r => r?,
            }
        }
    };
    let del = DeleteQuery::new(junction, WhereExpr::and_predicates(owner));
    let deleted = super::executor::delete_tx(&mut tx, &del).await?;
    super::executor::bulk_insert_tx(&mut tx, &ins).await?;
    tx.commit().await.map_err(ExecError::Driver)?;
    // Old keys unreadable: the change is unknown, so report no diff.
    let Some(old) = old else {
        let changed = !dsts.is_empty() || deleted > 0;
        return Ok(changed.then(SetDiff::default));
    };
    let key = SqlValue::to_display_string;
    let new: std::collections::HashSet<String> = dsts.iter().map(key).collect();
    let was: std::collections::HashSet<String> = old.iter().map(key).collect();
    // A keyless junction can hold one link twice: report it once.
    let mut seen = std::collections::HashSet::new();
    let diff = SetDiff {
        added: dsts
            .iter()
            .filter(|d| !was.contains(&key(d)))
            .cloned()
            .collect(),
        removed: old
            .into_iter()
            .filter(|o| !new.contains(&key(o)) && seen.insert(key(o)))
            .collect(),
    };
    Ok((!diff.added.is_empty() || !diff.removed.is_empty()).then_some(diff))
}

/// The projected key column, bound as `dst_key` binds; `None` for a type it cannot decode.
async fn keys_on(
    tx: &mut crate::sql::PoolTx<'_>,
    query: &SelectQuery,
    ty: Option<FieldType>,
) -> Result<Option<Vec<SqlValue>>, ExecError> {
    use super::executor::fetch_values_flat_on as fetch;
    let keys: Vec<SqlValue> = match ty {
        // PG decodes only the exact integer width.
        Some(FieldType::I16) => wide(fetch::<i16, _>(tx, query).await?),
        Some(FieldType::I32) => wide(fetch::<i32, _>(tx, query).await?),
        Some(FieldType::I64) => wide(fetch::<i64, _>(tx, query).await?),
        Some(FieldType::String) => values(fetch::<String, _>(tx, query).await?),
        Some(FieldType::Uuid) => values(fetch::<uuid::Uuid, _>(tx, query).await?),
        _ => return Ok(None),
    };
    Ok(Some(keys))
}

fn wide<K: Into<i64>>(keys: Vec<K>) -> Vec<SqlValue> {
    keys.into_iter().map(|k| SqlValue::I64(k.into())).collect()
}

fn values<K: Into<SqlValue>>(keys: Vec<K>) -> Vec<SqlValue> {
    keys.into_iter().map(Into::into).collect()
}

/// The source PK bound as-is (#1926); an unsaved source has none.
fn src_key(pk: &SqlValue, through: &'static str) -> Result<SqlValue, ExecError> {
    match pk {
        SqlValue::Null => Err(ExecError::M2mUnsavedSource { through }),
        v => Ok(v.clone()),
    }
}

/// [`dst_key`] for each id, first occurrence kept: a repeated id
/// (`tags=1&tags=1`) would hit the junction's unique key in `set` (#2297).
/// Exact match only: on MySQL's case-insensitive collation `"a"` and `"A"`
/// still collide.
fn dst_keys<K: Clone + Into<SqlValue>>(
    ids: &[K],
    through: &'static str,
    dst_col: &'static str,
) -> Result<Vec<SqlValue>, ExecError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for k in ids {
        let d = dst_key(k.clone(), through, dst_col)?;
        if seen.insert(d.to_display_string()) {
            out.push(d);
        }
    }
    Ok(out)
}

/// A destination key bound as the `dst` column's type (#1950): integers widen to
/// `i64`, a parseable string converts, any other mismatch is an error, not a PG 500.
fn dst_key(
    v: impl Into<SqlValue>,
    through: &'static str,
    dst_col: &'static str,
) -> Result<SqlValue, ExecError> {
    let v = match v.into() {
        SqlValue::I16(n) => SqlValue::I64(n.into()),
        SqlValue::I32(n) => SqlValue::I64(n.into()),
        v => v,
    };
    let Some(want) = dst_column_type(through, dst_col) else {
        return Ok(v);
    };
    let Some(got) = v.field_type() else {
        return Ok(v);
    };
    let is_int = |t: FieldType| matches!(t, FieldType::I16 | FieldType::I32 | FieldType::I64);
    if got == want || (is_int(got) && is_int(want)) {
        return Ok(v);
    }
    let converted = match &v {
        SqlValue::String(s) if is_int(want) => s.parse::<i64>().ok().map(SqlValue::I64),
        SqlValue::String(s) if want == FieldType::Uuid => {
            uuid::Uuid::parse_str(s).ok().map(SqlValue::Uuid)
        }
        _ => None,
    };
    converted.ok_or_else(|| {
        ExecError::Query(QueryError::TypeMismatch {
            model: through,
            field: dst_col.to_owned(),
            expected: want,
            actual: got,
        })
    })
}

/// The `dst` column's type: from the registered through model, else as the
/// migration writer creates the junction. `None` when neither is known.
fn dst_column_type(through: &str, dst_col: &str) -> Option<FieldType> {
    if let Some(entry) = crate::core::ModelEntry::for_table(through) {
        return entry.schema.field_by_column(dst_col).map(|f| f.ty);
    }
    auto_column_type(through, dst_col)
}

// ============================================================ polymorphic (generic) M2M

/// Manages the rows in a **polymorphic** junction table for one source
/// instance — Eloquent's `morphToMany` / `morphedByMany` (issue #818).
///
/// Unlike [`M2MManager`], the pivot carries a ContentType discriminator
/// so two unrelated models (e.g. `Post` and `Video`) can share one
/// junction (`taggables(tag_id, taggable_id, taggable_type)`) and one
/// related set (`Tag`). Every query is scoped to the owning model's
/// `content_type_id`, resolved from [`Self::src_schema`] via
/// [`crate::contenttypes::ContentType::get_for_schema`] (cached).
///
/// The `content_type_id` column is bound as an `i64`; a non-integer one is
/// not supported yet (#2050).
///
/// Constructed by the macro-generated `<name>_m2m()` method on any model
/// declaring `#[rustango(generic_m2m(...))]` — do not build directly.
pub struct GenericM2MManager {
    /// PK value of the owning model instance (the `pk_col` value).
    pub src_pk: SqlValue,
    /// Schema of the owning model — used to resolve its `content_type_id`
    /// (the `ct_col` value) at call time.
    pub src_schema: &'static crate::core::ModelSchema,
    /// SQL name of the polymorphic junction table (e.g. `"taggables"`).
    pub through: &'static str,
    /// Junction column holding the owning instance's PK (e.g. `"taggable_id"`).
    pub pk_col: &'static str,
    /// Junction column holding the owning model's `content_type_id`
    /// discriminator (e.g. `"taggable_type"`).
    pub ct_col: &'static str,
    /// Junction column holding the related model's PK (e.g. `"tag_id"`).
    pub dst_col: &'static str,
}

/// The junction deletes an owner delete must run first: rows whose
/// content type is the owner's and whose pk is one the delete removes.
/// Reads the owner and the content-type table, never the junction, so
/// MySQL accepts it (1093).
pub(crate) fn generic_m2m_links(query: &DeleteQuery) -> Vec<DeleteQuery> {
    let owner = query.model;
    let mut entries = inventory::iter::<crate::core::GenericM2MEntry>
        .into_iter()
        .filter(|e| e.owner.table == owner.table)
        .peekable();
    if entries.peek().is_none() {
        return Vec::new();
    }
    let (Some(pk), Some(model)) = (
        owner.primary_key(),
        crate::core::ModelEntry::for_schema(owner),
    ) else {
        return Vec::new();
    };
    let app = model.resolved_app_label().unwrap_or("project");
    let name = owner.name.to_ascii_lowercase();
    let ct = <crate::contenttypes::ContentType as crate::core::Model>::SCHEMA;
    let mut ct_ids = SelectQuery::new(ct).where_clause(WhereExpr::and_predicates(vec![
        eq("app_label", SqlValue::String(app.to_owned())),
        eq("model_name", SqlValue::String(name)),
    ]));
    ct_ids.projection = Some(vec!["id"]);
    let mut owner_pks = SelectQuery::new(owner).where_clause(query.where_clause.clone());
    owner_pks.projection = Some(vec![pk.column]);
    // A table-wide delete clears every row of the owner's type, orphans too.
    let every_row = query.where_clause.is_empty();
    entries
        .map(|e| {
            let mut preds = vec![WhereExpr::InSubquery {
                column: e.ct_col,
                negated: false,
                subquery: Box::new(ct_ids.clone()),
            }];
            if !every_row {
                preds.push(WhereExpr::InSubquery {
                    column: e.pk_col,
                    negated: false,
                    subquery: Box::new(owner_pks.clone()),
                });
            }
            DeleteQuery::new(
                junction(e.through, &[e.pk_col, e.ct_col]),
                WhereExpr::And(preds),
            )
        })
        .collect()
}

/// The junction deletes that emptying `table` must run first.
pub(crate) fn generic_m2m_truncate_links(table: &str) -> Vec<DeleteQuery> {
    inventory::iter::<crate::core::GenericM2MEntry>
        .into_iter()
        .find(|e| e.owner.table == table)
        .map(|e| generic_m2m_links(&DeleteQuery::new(e.owner, WhereExpr::And(Vec::new()))))
        .unwrap_or_default()
}

impl GenericM2MManager {
    fn src_key(&self) -> Result<SqlValue, ExecError> {
        src_key(&self.src_pk, self.through)
    }

    fn junction(&self) -> &'static ModelSchema {
        junction(self.through, &[self.pk_col, self.ct_col, self.dst_col])
    }

    /// The `pk_col` / `ct_col` filters that pick this instance's rows.
    async fn owner(&self, pool: &Pool) -> Result<Vec<Filter>, ExecError> {
        Ok(self.owner_ct(pool).await?.0)
    }

    /// [`Self::owner`] with the content type id it resolved.
    async fn owner_ct(&self, pool: &Pool) -> Result<(Vec<Filter>, i64), ExecError> {
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        let filters = vec![eq(self.pk_col, src), eq(self.ct_col, SqlValue::I64(ct))];
        Ok((filters, ct))
    }

    /// Resolve the owning model's `content_type_id`, erroring clearly
    /// when the content type hasn't been seeded.
    async fn ct_id(&self, pool: &Pool) -> Result<i64, ExecError> {
        crate::contenttypes::ContentType::get_for_schema(pool, self.src_schema)
            .await?
            .and_then(|ct| ct.id.get().copied())
            .ok_or(ExecError::ContentTypeNotRegistered {
                table: self.src_schema.table,
            })
    }

    /// Fire `m2m_changed` for a junction mutation (reuses the monomorphic
    /// signal context; `src_col` reports the polymorphic `pk_col`).
    ///
    /// Gated with its call sites (#1208) — the parameter type comes from the
    /// optional `signals` module, so the signature itself cannot exist without
    /// the feature.
    #[cfg(feature = "signals")]
    async fn signal(
        &self,
        action: crate::signals::m2m::M2mAction,
        ct: i64,
        dst_pks: Vec<SqlValue>,
        diff: SetDiff,
    ) {
        let ctx = crate::signals::m2m::M2mChangedContext::new(
            action,
            self.through,
            self.pk_col,
            self.dst_col,
            self.src_pk.clone(),
            dst_pks,
        )
        .with_content_type(ct)
        .with_diff(diff.added, diff.removed);
        crate::signals::m2m::send_m2m_changed(ctx).await;
    }

    /// Related PKs linked to this instance (scoped to its content type),
    /// decoded as the target's key type `K`.
    ///
    /// # Errors
    /// Driver failures, a `dst` column that does not decode as `K`, or
    /// [`ExecError::ContentTypeNotRegistered`].
    pub async fn all<K: FlatScalar>(&self, pool: &Pool) -> Result<Vec<K>, ExecError> {
        let owner = self.owner(pool).await?;
        fetch_dst(self.junction(), self.dst_col, owner, pool).await
    }

    /// Link `dst_id` to this instance. No-op if already present.
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn add(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let (owner, ct) = self.owner_ct(pool).await?;
        let mut values: Vec<SqlValue> = owner.into_iter().map(|f| f.value).collect();
        values.push(dst.clone());
        let query = InsertQuery::new(
            self.junction(),
            vec![self.pk_col, self.ct_col, self.dst_col],
            values,
        );
        if !super::executor::insert_or_ignore(pool, &query).await? {
            return Ok(());
        }
        #[cfg(feature = "signals")]
        self.signal(
            crate::signals::m2m::M2mAction::Add,
            ct,
            vec![dst.clone()],
            SetDiff {
                added: vec![dst],
                removed: Vec::new(),
            },
        )
        .await;
        #[cfg(not(feature = "signals"))]
        let _ = ct;
        Ok(())
    }

    /// Unlink `dst_id` from this instance. No-op if not present.
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn remove(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let (mut filters, ct) = self.owner_ct(pool).await?;
        filters.push(eq(self.dst_col, dst.clone()));
        if delete(self.junction(), filters, pool).await? == 0 {
            return Ok(());
        }
        #[cfg(feature = "signals")]
        self.signal(
            crate::signals::m2m::M2mAction::Remove,
            ct,
            vec![dst.clone()],
            SetDiff {
                added: Vec::new(),
                removed: vec![dst],
            },
        )
        .await;
        #[cfg(not(feature = "signals"))]
        let _ = ct;
        Ok(())
    }

    /// Replace the full linked set with `ids` — atomic DELETE + INSERT in
    /// one transaction, scoped to this instance's content type.
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn set<K: Clone + Into<SqlValue>>(
        &self,
        ids: &[K],
        pool: &Pool,
    ) -> Result<(), ExecError> {
        let dsts = dst_keys(ids, self.through, self.dst_col)?;
        let (owner, ct) = self.owner_ct(pool).await?;
        let rows = dsts
            .iter()
            .map(|d| {
                owner
                    .iter()
                    .map(|f| f.value.clone())
                    .chain([d.clone()])
                    .collect()
            })
            .collect();
        let columns = vec![self.pk_col, self.ct_col, self.dst_col];
        let junction = self.junction();
        let locks = source_lock(self.src_schema, &self.src_key()?)
            .into_iter()
            .collect();
        let Some(diff) = replace(
            locks,
            junction,
            owner,
            self.dst_col,
            &dsts,
            columns,
            rows,
            pool,
        )
        .await?
        else {
            return Ok(());
        };
        #[cfg(feature = "signals")]
        self.signal(crate::signals::m2m::M2mAction::Set, ct, dsts, diff)
            .await;
        #[cfg(not(feature = "signals"))]
        let _ = (ct, diff);
        Ok(())
    }

    /// Unlink every related row for this instance (scoped to its CT).
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn clear(&self, pool: &Pool) -> Result<(), ExecError> {
        let (owner, ct) = self.owner_ct(pool).await?;
        if delete(self.junction(), owner, pool).await? == 0 {
            return Ok(());
        }
        #[cfg(feature = "signals")]
        self.signal(
            crate::signals::m2m::M2mAction::Clear,
            ct,
            Vec::new(),
            SetDiff::default(),
        )
        .await;
        #[cfg(not(feature = "signals"))]
        let _ = ct;
        Ok(())
    }

    /// `true` if `dst_id` is linked to this instance (within its CT).
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn contains(
        &self,
        dst_id: impl Into<SqlValue>,
        pool: &Pool,
    ) -> Result<bool, ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let mut filters = self.owner(pool).await?;
        filters.push(eq(self.dst_col, dst));
        exists(self.junction(), filters, pool).await
    }
}
