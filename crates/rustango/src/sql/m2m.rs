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

use super::error::ExecError;
use super::{FlatScalar, Pool};
use crate::core::{FieldType, QueryError, SqlValue};

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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let sql = format!(
            "SELECT {dst} FROM {through} WHERE {src} = {p1}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
        );
        let binds = vec![src.clone()];
        fetch_col_pool::<K>(pool, &sql, binds).await
    }

    /// Add `dst_id` to the junction table. No-op if already present.
    /// Tri-dialect: uses `INSERT … ON CONFLICT DO NOTHING` on
    /// Postgres + SQLite (both support it ≥ SQLite 3.24), and a no-op
    /// `ON DUPLICATE KEY UPDATE` on MySQL.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn add(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let src_q = dialect.quote_ident(self.src_col);
        let sql = format!(
            "INSERT INTO {through} ({src_q}, {dst}) VALUES ({p1}, {p2}) {skip}",
            through = dialect.quote_ident(self.through),
            dst = dialect.quote_ident(self.dst_col),
            skip = skip_duplicate(dialect, &src_q),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![src.clone(), dst.clone()];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let sql = format!(
            "DELETE FROM {through} WHERE {src} = {p1} AND {dst} = {p2}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![src.clone(), dst.clone()];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let del_sql = format!(
            "DELETE FROM {through} WHERE {src} = {p1}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            p1 = dialect.placeholder(1),
        );
        // Build multi-row INSERT only when ids is non-empty (otherwise
        // we'd emit `VALUES ()` which every backend rejects).
        let dsts = ids
            .iter()
            .map(|k| dst_key(k.clone(), self.through, self.dst_col))
            .collect::<Result<Vec<_>, _>>()?;
        let ins_sql_with_binds = if dsts.is_empty() {
            None
        } else {
            let mut sql = format!(
                "INSERT INTO {through} ({src}, {dst}) VALUES ",
                through = dialect.quote_ident(self.through),
                src = dialect.quote_ident(self.src_col),
                dst = dialect.quote_ident(self.dst_col),
            );
            let mut binds = Vec::with_capacity(ids.len() * 2);
            for (i, dst) in dsts.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let p_src = dialect.placeholder(i * 2 + 1);
                let p_dst = dialect.placeholder(i * 2 + 2);
                sql.push_str(&format!("({p_src}, {p_dst})"));
                binds.push(src.clone());
                binds.push(dst.clone());
            }
            Some((sql, binds))
        };
        // #561 — was a 3-arm match each running DELETE + (optional)
        // INSERT inside a per-backend tx with local `bind_pg/my/sqlite`
        // helpers. The new `raw_execute_tx` combinator (#798) routes
        // the bind through the canonical executor `bind_query*` path,
        // so the body collapses to one flat sequence.
        let mut tx = crate::sql::transaction_pool(pool).await?;
        crate::sql::raw_execute_tx(&mut tx, &del_sql, vec![src.clone()]).await?;
        if let Some((ins_sql, binds)) = ins_sql_with_binds {
            crate::sql::raw_execute_tx(&mut tx, &ins_sql, binds).await?;
        }
        tx.commit().await.map_err(ExecError::Driver)?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let sql = format!(
            "DELETE FROM {through} WHERE {src} = {p1}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            p1 = dialect.placeholder(1),
        );
        let binds = vec![src.clone()];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        // COUNT(*) is a bigint on every backend, whatever the `dst` type (#1950).
        let sql = format!(
            "SELECT COUNT(*) FROM {through} WHERE {src} = {p1} AND {dst} = {p2}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![src.clone(), dst.clone()];
        let rows = fetch_col_pool::<i64>(pool, &sql, binds).await?;
        Ok(rows.first().is_some_and(|n| *n > 0))
    }

    fn src_key(&self) -> Result<SqlValue, ExecError> {
        src_key(&self.src_pk, self.through)
    }
}

/// Skip an existing link. Untargeted off MySQL: a `through` model need not have a
/// unique pair. MySQL's `INSERT IGNORE` also hid truncation and FK errors (#1966).
fn skip_duplicate(dialect: &dyn super::Dialect, pivot: &str) -> String {
    if dialect.name() == "mysql" {
        dialect.insert_on_conflict_skip(&[pivot])
    } else {
        "ON CONFLICT DO NOTHING".to_owned()
    }
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        let sql = format!(
            "SELECT {dst} FROM {through} WHERE {pk} = {p1} AND {ctc} = {p2}",
            through = dialect.quote_ident(self.through),
            pk = dialect.quote_ident(self.pk_col),
            ctc = dialect.quote_ident(self.ct_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![src.clone(), SqlValue::I64(ct)];
        fetch_col_pool::<K>(pool, &sql, binds).await
    }

    /// Link `dst_id` to this instance. No-op if already present.
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn add(&self, dst_id: impl Into<SqlValue>, pool: &Pool) -> Result<(), ExecError> {
        let dst = dst_key(dst_id, self.through, self.dst_col)?;
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        let pk = dialect.quote_ident(self.pk_col);
        let sql = format!(
            "INSERT INTO {through} ({pk}, {ctc}, {dst}) VALUES ({p1}, {p2}, {p3}) {skip}",
            through = dialect.quote_ident(self.through),
            skip = skip_duplicate(dialect, &pk),
            ctc = dialect.quote_ident(self.ct_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
            p3 = dialect.placeholder(3),
        );
        let binds = vec![src.clone(), SqlValue::I64(ct), dst.clone()];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        let sql = format!(
            "DELETE FROM {through} WHERE {pk} = {p1} AND {ctc} = {p2} AND {dst} = {p3}",
            through = dialect.quote_ident(self.through),
            pk = dialect.quote_ident(self.pk_col),
            ctc = dialect.quote_ident(self.ct_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
            p3 = dialect.placeholder(3),
        );
        let binds = vec![src.clone(), SqlValue::I64(ct), dst.clone()];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        let del_sql = format!(
            "DELETE FROM {through} WHERE {pk} = {p1} AND {ctc} = {p2}",
            through = dialect.quote_ident(self.through),
            pk = dialect.quote_ident(self.pk_col),
            ctc = dialect.quote_ident(self.ct_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let dsts = ids
            .iter()
            .map(|k| dst_key(k.clone(), self.through, self.dst_col))
            .collect::<Result<Vec<_>, _>>()?;
        let ins = if dsts.is_empty() {
            None
        } else {
            let mut sql = format!(
                "INSERT INTO {through} ({pk}, {ctc}, {dst}) VALUES ",
                through = dialect.quote_ident(self.through),
                pk = dialect.quote_ident(self.pk_col),
                ctc = dialect.quote_ident(self.ct_col),
                dst = dialect.quote_ident(self.dst_col),
            );
            let mut binds = Vec::with_capacity(ids.len() * 3);
            for (i, dst) in dsts.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let p1 = dialect.placeholder(i * 3 + 1);
                let p2 = dialect.placeholder(i * 3 + 2);
                let p3 = dialect.placeholder(i * 3 + 3);
                sql.push_str(&format!("({p1}, {p2}, {p3})"));
                binds.push(src.clone());
                binds.push(SqlValue::I64(ct));
                binds.push(dst.clone());
            }
            Some((sql, binds))
        };
        let mut tx = crate::sql::transaction_pool(pool).await?;
        crate::sql::raw_execute_tx(&mut tx, &del_sql, vec![src.clone(), SqlValue::I64(ct)]).await?;
        if let Some((ins_sql, binds)) = ins {
            crate::sql::raw_execute_tx(&mut tx, &ins_sql, binds).await?;
        }
        tx.commit().await.map_err(ExecError::Driver)?;
        #[cfg(feature = "signals")]
        self.signal(crate::signals::m2m::M2mAction::Set, dsts).await;
        Ok(())
    }

    /// Unlink every related row for this instance (scoped to its CT).
    ///
    /// # Errors
    /// Driver failures, or [`ExecError::ContentTypeNotRegistered`].
    pub async fn clear(&self, pool: &Pool) -> Result<(), ExecError> {
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        let sql = format!(
            "DELETE FROM {through} WHERE {pk} = {p1} AND {ctc} = {p2}",
            through = dialect.quote_ident(self.through),
            pk = dialect.quote_ident(self.pk_col),
            ctc = dialect.quote_ident(self.ct_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![src.clone(), SqlValue::I64(ct)];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
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
        let dialect = pool.dialect();
        let src = self.src_key()?;
        let ct = self.ct_id(pool).await?;
        // COUNT(*) is a bigint on every backend, whatever the `dst` type (#1950).
        let sql = format!(
            "SELECT COUNT(*) FROM {through} WHERE {pk} = {p1} AND {ctc} = {p2} AND {dst} = {p3}",
            through = dialect.quote_ident(self.through),
            pk = dialect.quote_ident(self.pk_col),
            ctc = dialect.quote_ident(self.ct_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
            p3 = dialect.placeholder(3),
        );
        let binds = vec![src.clone(), SqlValue::I64(ct), dst.clone()];
        let rows = fetch_col_pool::<i64>(pool, &sql, binds).await?;
        Ok(rows.first().is_some_and(|n| *n > 0))
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

// ============================================================ small per-backend helpers

/// Run a single-column SELECT and decode each row's value as `K`. `FlatScalar`
/// reads a UUID from MySQL's `CHAR(36)` and SQLite text or bytes (#1950).
async fn fetch_col_pool<K: FlatScalar>(
    pool: &Pool,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<Vec<K>, ExecError> {
    crate::test_assertions::query_counter::bump();
    super::executor::fetch_flat_raw(pool, sql, binds).await
}

// #561 — the three local `bind_pg`/`bind_my`/`bind_sqlite` helpers
// were removed once `set_pool` started routing through
// `raw_execute_tx` (which uses the canonical executor `bind_query*`
// path). The audit-tx + m2m-set body is now a single flat sequence.
