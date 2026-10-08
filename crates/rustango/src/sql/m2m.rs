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
    /// Return all destination PKs linked to the source instance.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn all(&self, pool: &Pool) -> Result<Vec<i64>, ExecError> {
        self.all_as::<i64>(pool).await
    }

    /// [`Self::all`] decoded as `K`, for a `String` / `Uuid` target PK (#1950).
    ///
    /// # Errors
    /// Driver failures, including a `dst` column that does not decode as `K`.
    pub async fn all_as<K: FlatScalar>(&self, pool: &Pool) -> Result<Vec<K>, ExecError> {
        let src = self.src_key()?;
        fetch_dst(
            self.junction(),
            self.dst_col,
            vec![eq(self.src_col, src)],
            pool,
        )
        .await
    }

    /// Add `dst_id` to the junction table. No-op if already present.
    /// Tri-dialect: [`insert_or_ignore`](super::insert_or_ignore); a duplicate fires no signal.
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
        // A skipped duplicate changed nothing, so it fires no signal (#2221).
        if !super::executor::insert_or_ignore(pool, &query).await? {
            return Ok(());
        }
        // #410 — fire m2m_changed after successful junction-row write.
        // `signals` is an optional feature, so every emission below is gated
        // (#1208) — these statements were unconditional while the module is not.
        #[cfg(feature = "signals")]
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Add,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: src,
            dst_pks: vec![dst],
        })
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
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Remove,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: src,
            dst_pks: vec![dst],
        })
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
        let dsts = ids
            .iter()
            .map(|k| dst_key(k.clone(), self.through, self.dst_col))
            .collect::<Result<Vec<_>, _>>()?;
        let rows = dsts.iter().map(|d| vec![src.clone(), d.clone()]).collect();
        replace(
            self.junction(),
            vec![eq(self.src_col, src.clone())],
            vec![self.src_col, self.dst_col],
            rows,
            pool,
        )
        .await?;
        // #410 — fire m2m_changed after the atomic DELETE+INSERT
        // commits. `dst_pks` is the new full set (may be empty when
        // `set([])` was called).
        #[cfg(feature = "signals")]
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Set,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: src,
            dst_pks: dsts,
        })
        .await;
        Ok(())
    }

    /// Remove all junction rows for the source instance.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn clear(&self, pool: &Pool) -> Result<(), ExecError> {
        let src = self.src_key()?;
        delete(self.junction(), vec![eq(self.src_col, src.clone())], pool).await?;
        // #410 — fire m2m_changed after clear.
        #[cfg(feature = "signals")]
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Clear,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: src,
            dst_pks: Vec::new(),
        })
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

    fn junction(&self) -> &'static ModelSchema {
        junction(self.through, &[self.src_col, self.dst_col])
    }
}

/// The junction as a schema the emitters compile against (#2136): the
/// registered through model, else one naming just the manager's columns.
pub(crate) fn junction(through: &'static str, cols: &[&'static str]) -> &'static ModelSchema {
    type Key = (&'static str, Vec<&'static str>);
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
        // Types only pick NULL casts, and a junction never binds NULL.
        let fields: Vec<FieldSchema> = cols
            .iter()
            .map(|c| FieldSchema::new(c, c, FieldType::I64))
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
    crate::test_assertions::query_counter::bump();
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

/// Delete the rows `owner` matches and insert `rows` in one transaction, so
/// readers never see the empty state in between.
async fn replace(
    junction: &'static ModelSchema,
    owner: Vec<Filter>,
    columns: Vec<&'static str>,
    rows: Vec<Vec<SqlValue>>,
    pool: &Pool,
) -> Result<(), ExecError> {
    // A bad key fails before the DELETE runs, not after.
    let ins = BulkInsertQuery::new(junction, columns, rows);
    ins.validate()?;
    let mut tx = crate::sql::transaction_pool(pool).await?;
    let del = DeleteQuery::new(junction, WhereExpr::and_predicates(owner));
    super::executor::delete_tx(&mut tx, &del).await?;
    super::executor::bulk_insert_tx(&mut tx, &ins).await?;
    tx.commit().await.map_err(ExecError::Driver)?;
    Ok(())
}

/// The source PK bound as-is (#1926); an unsaved source has none.
fn src_key(pk: &SqlValue, through: &'static str) -> Result<SqlValue, ExecError> {
    match pk {
        SqlValue::Null => Err(ExecError::M2mUnsavedSource { through }),
        v => Ok(v.clone()),
    }
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

/// The `dst` column's type: from the registered through model, else BIGINT for
/// a junction the migration writer creates. `None` when neither is known.
fn dst_column_type(through: &str, dst_col: &str) -> Option<FieldType> {
    if let Some(entry) = crate::core::ModelEntry::for_table(through) {
        return entry.schema.field_by_column(dst_col).map(|f| f.ty);
    }
    inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .flat_map(|e| e.schema.m2m)
        .any(|r| r.auto_create && r.through == through && r.dst_col == dst_col)
        .then_some(FieldType::I64)
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

impl GenericM2MManager {
    fn src_key(&self) -> Result<SqlValue, ExecError> {
        src_key(&self.src_pk, self.through)
    }

    fn junction(&self) -> &'static ModelSchema {
        junction(self.through, &[self.pk_col, self.ct_col, self.dst_col])
    }

    /// The `pk_col` / `ct_col` filters that pick this instance's rows.
    async fn owner(&self, pool: &Pool) -> Result<Vec<Filter>, ExecError> {
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        Ok(vec![
            eq(self.pk_col, src),
            eq(self.ct_col, SqlValue::I64(ct)),
        ])
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
    async fn signal(&self, action: crate::signals::m2m::M2mAction, dst_pks: Vec<SqlValue>) {
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action,
            through: self.through,
            src_col: self.pk_col,
            dst_col: self.dst_col,
            src_pk: self.src_pk.clone(),
            dst_pks,
        })
        .await;
    }

    /// Related PKs linked to this instance (scoped to its content type).
    /// Decoded as `i64`; use [`Self::all_as`] for a non-integer target PK (#2050).
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn all(&self, pool: &Pool) -> Result<Vec<i64>, ExecError> {
        self.all_as::<i64>(pool).await
    }

    /// [`Self::all`] decoded as `K`, for a `String` / `Uuid` target PK (#1950).
    ///
    /// # Errors
    /// Driver failures, including a `dst` column that does not decode as `K`.
    pub async fn all_as<K: FlatScalar>(&self, pool: &Pool) -> Result<Vec<K>, ExecError> {
        let owner = self.owner(pool).await?;
        fetch_dst(self.junction(), self.dst_col, owner, pool).await
    }

    /// Link `dst_id` to this instance. No-op if already present.
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn add(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let mut values: Vec<SqlValue> = self
            .owner(pool)
            .await?
            .into_iter()
            .map(|f| f.value)
            .collect();
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
        self.signal(crate::signals::m2m::M2mAction::Add, vec![dst])
            .await;
        Ok(())
    }

    /// Unlink `dst_id` from this instance. No-op if not present.
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn remove(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let mut filters = self.owner(pool).await?;
        filters.push(eq(self.dst_col, dst.clone()));
        if delete(self.junction(), filters, pool).await? == 0 {
            return Ok(());
        }
        #[cfg(feature = "signals")]
        self.signal(crate::signals::m2m::M2mAction::Remove, vec![dst])
            .await;
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
        let dsts = ids
            .iter()
            .map(|k| dst_key(k.clone(), self.through, self.dst_col))
            .collect::<Result<Vec<_>, _>>()?;
        let owner = self.owner(pool).await?;
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
        replace(self.junction(), owner, columns, rows, pool).await?;
        #[cfg(feature = "signals")]
        self.signal(crate::signals::m2m::M2mAction::Set, dsts).await;
        Ok(())
    }

    /// Unlink every related row for this instance (scoped to its CT).
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn clear(&self, pool: &Pool) -> Result<(), ExecError> {
        let owner = self.owner(pool).await?;
        delete(self.junction(), owner, pool).await?;
        #[cfg(feature = "signals")]
        self.signal(crate::signals::m2m::M2mAction::Clear, Vec::new())
            .await;
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

// ============================================================ deprecated _pool aliases

/// Source-compat shims for callers still using the pre-#891
/// `_pool`-suffixed names. Each forwards verbatim to the bare-name
/// method above. Slated for removal in the next major version —
/// the deprecation attribute is the canary.
impl M2MManager {
    #[deprecated(note = "renamed to `all` — drop the `_pool` suffix")]
    pub async fn all_pool(&self, pool: &Pool) -> Result<Vec<i64>, ExecError> {
        self.all(pool).await
    }

    #[deprecated(note = "renamed to `add` — drop the `_pool` suffix")]
    pub async fn add_pool(&self, dst_id: i64, pool: &Pool) -> Result<(), ExecError> {
        self.add(dst_id, pool).await
    }

    #[deprecated(note = "renamed to `remove` — drop the `_pool` suffix")]
    pub async fn remove_pool(&self, dst_id: i64, pool: &Pool) -> Result<(), ExecError> {
        self.remove(dst_id, pool).await
    }

    #[deprecated(note = "renamed to `set` — drop the `_pool` suffix")]
    pub async fn set_pool(&self, ids: &[i64], pool: &Pool) -> Result<(), ExecError> {
        self.set(ids, pool).await
    }

    #[deprecated(note = "renamed to `clear` — drop the `_pool` suffix")]
    pub async fn clear_pool(&self, pool: &Pool) -> Result<(), ExecError> {
        self.clear(pool).await
    }

    #[deprecated(note = "renamed to `contains` — drop the `_pool` suffix")]
    pub async fn contains_pool(&self, dst_id: i64, pool: &Pool) -> Result<bool, ExecError> {
        self.contains(dst_id, pool).await
    }
}
