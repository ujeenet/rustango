//! Many-to-many manager — CRUD operations on junction tables.
//!
//! Obtain an instance via the macro-generated `<name>_m2m()` method on any
//! model that declares a `#[rustango(m2m(...))]` relation.
//!
//! # Example
//!
//! ```ignore
//! // Fetch all tag IDs for a post:
//! let tag_ids = post.tags_m2m().all_pool(&pool).await?;
//!
//! // Add a tag:
//! post.tags_m2m().add_pool(42, &pool).await?;
//!
//! // Remove a tag:
//! post.tags_m2m().remove_pool(42, &pool).await?;
//!
//! // Replace all tags:
//! post.tags_m2m().set_pool(&[1, 2, 3], &pool).await?;
//!
//! // Clear all tags:
//! post.tags_m2m().clear_pool(&pool).await?;
//!
//! // Check membership:
//! let has = post.tags_m2m().contains_pool(42, &pool).await?;
//! ```
//!
//! ## Backend coverage (v0.35 tri-dialect)
//!
//! Every method has a `*_pool(&Pool, …)` form that dispatches per-backend
//! through [`crate::sql::Pool`]. The legacy `&PgPool` signatures are
//! one-line shims that delegate to the `_pool` variant via
//! `Pool::Postgres(pool.clone())` — kept for source-compat with v0.34
//! and earlier.

use super::error::ExecError;
use super::Pool;
use crate::core::SqlValue;

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
    pub async fn all_pool(&self, pool: &Pool) -> Result<Vec<i64>, ExecError> {
        let dialect = pool.dialect();
        let sql = format!(
            "SELECT {dst} FROM {through} WHERE {src} = {p1}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
        );
        let binds = vec![SqlValue::I64(self.src_pk_i64())];
        fetch_i64_col_pool(pool, &sql, binds, self.dst_col).await
    }

    /// Add `dst_id` to the junction table. No-op if already present.
    /// Tri-dialect: uses `INSERT … ON CONFLICT DO NOTHING` on
    /// Postgres + SQLite (both support it ≥ SQLite 3.24), and
    /// `INSERT IGNORE INTO …` on MySQL.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn add_pool(&self, dst_id: i64, pool: &Pool) -> Result<(), ExecError> {
        let dialect = pool.dialect();
        let (insert_kw, suffix) = match dialect.name() {
            "mysql" => ("INSERT IGNORE INTO", ""),
            _ => ("INSERT INTO", " ON CONFLICT DO NOTHING"),
        };
        let sql = format!(
            "{insert_kw} {through} ({src}, {dst}) VALUES ({p1}, {p2}){suffix}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![SqlValue::I64(self.src_pk_i64()), SqlValue::I64(dst_id)];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
        // #410 — fire m2m_changed after successful junction-row write.
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Add,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: self.src_pk_i64(),
            dst_pks: vec![dst_id],
        })
        .await;
        Ok(())
    }

    /// Remove `dst_id` from the junction table. No-op if not present.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn remove_pool(&self, dst_id: i64, pool: &Pool) -> Result<(), ExecError> {
        let dialect = pool.dialect();
        let sql = format!(
            "DELETE FROM {through} WHERE {src} = {p1} AND {dst} = {p2}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![SqlValue::I64(self.src_pk_i64()), SqlValue::I64(dst_id)];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
        // #410 — fire m2m_changed after successful junction-row remove.
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Remove,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: self.src_pk_i64(),
            dst_pks: vec![dst_id],
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
    pub async fn set_pool(&self, ids: &[i64], pool: &Pool) -> Result<(), ExecError> {
        let dialect = pool.dialect();
        let del_sql = format!(
            "DELETE FROM {through} WHERE {src} = {p1}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            p1 = dialect.placeholder(1),
        );
        // Build multi-row INSERT only when ids is non-empty (otherwise
        // we'd emit `VALUES ()` which every backend rejects).
        let ins_sql_with_binds = if ids.is_empty() {
            None
        } else {
            let mut sql = format!(
                "INSERT INTO {through} ({src}, {dst}) VALUES ",
                through = dialect.quote_ident(self.through),
                src = dialect.quote_ident(self.src_col),
                dst = dialect.quote_ident(self.dst_col),
            );
            let mut binds = Vec::with_capacity(ids.len() * 2);
            let src_pk = self.src_pk_i64();
            for (i, dst_id) in ids.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let p_src = dialect.placeholder(i * 2 + 1);
                let p_dst = dialect.placeholder(i * 2 + 2);
                sql.push_str(&format!("({p_src}, {p_dst})"));
                binds.push(SqlValue::I64(src_pk));
                binds.push(SqlValue::I64(*dst_id));
            }
            Some((sql, binds))
        };
        // #561 — was a 3-arm match each running DELETE + (optional)
        // INSERT inside a per-backend tx with local `bind_pg/my/sqlite`
        // helpers. The new `raw_execute_tx` combinator (#798) routes
        // the bind through the canonical executor `bind_query*` path,
        // so the body collapses to one flat sequence.
        let mut tx = crate::sql::transaction_pool(pool).await?;
        crate::sql::raw_execute_tx(&mut tx, &del_sql, vec![SqlValue::I64(self.src_pk_i64())])
            .await?;
        if let Some((ins_sql, binds)) = ins_sql_with_binds {
            crate::sql::raw_execute_tx(&mut tx, &ins_sql, binds).await?;
        }
        tx.commit().await.map_err(ExecError::Driver)?;
        // #410 — fire m2m_changed after the atomic DELETE+INSERT
        // commits. `dst_pks` is the new full set (may be empty when
        // `set([])` was called).
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Set,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: self.src_pk_i64(),
            dst_pks: ids.to_vec(),
        })
        .await;
        Ok(())
    }

    /// Remove all junction rows for the source instance.
    /// Tri-dialect via [`Pool`] dispatch.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn clear_pool(&self, pool: &Pool) -> Result<(), ExecError> {
        let dialect = pool.dialect();
        let sql = format!(
            "DELETE FROM {through} WHERE {src} = {p1}",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            p1 = dialect.placeholder(1),
        );
        let binds = vec![SqlValue::I64(self.src_pk_i64())];
        super::executor::raw_execute_pool(pool, &sql, binds).await?;
        // #410 — fire m2m_changed after clear.
        crate::signals::m2m::send_m2m_changed(crate::signals::m2m::M2mChangedContext {
            action: crate::signals::m2m::M2mAction::Clear,
            through: self.through,
            src_col: self.src_col,
            dst_col: self.dst_col,
            src_pk: self.src_pk_i64(),
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
    pub async fn contains_pool(&self, dst_id: i64, pool: &Pool) -> Result<bool, ExecError> {
        let dialect = pool.dialect();
        let sql = format!(
            "SELECT 1 AS hit FROM {through} WHERE {src} = {p1} AND {dst} = {p2} LIMIT 1",
            through = dialect.quote_ident(self.through),
            src = dialect.quote_ident(self.src_col),
            dst = dialect.quote_ident(self.dst_col),
            p1 = dialect.placeholder(1),
            p2 = dialect.placeholder(2),
        );
        let binds = vec![SqlValue::I64(self.src_pk_i64()), SqlValue::I64(dst_id)];
        let rows = fetch_i64_col_pool(pool, &sql, binds, "hit").await?;
        Ok(!rows.is_empty())
    }

    fn src_pk_i64(&self) -> i64 {
        match &self.src_pk {
            SqlValue::I64(v) => *v,
            SqlValue::I32(v) => i64::from(*v),
            _ => 0,
        }
    }
}

// ============================================================ legacy PG-typed shims

/// PG-typed back-compat wrappers around the tri-dialect `_pool`
/// methods above. Each forwards through `Pool::Postgres(pool.clone())`
/// — Pool wraps an `Arc` internally so the conversion is a reference
/// bump.
#[cfg(feature = "postgres")]
impl M2MManager {
    /// Return all destination PKs linked to the source instance.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn all(&self, pool: &sqlx::PgPool) -> Result<Vec<i64>, ExecError> {
        self.all_pool(&Pool::Postgres(pool.clone())).await
    }

    /// Add `dst_id` to the junction table.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn add(&self, dst_id: i64, pool: &sqlx::PgPool) -> Result<(), ExecError> {
        self.add_pool(dst_id, &Pool::Postgres(pool.clone())).await
    }

    /// Remove `dst_id` from the junction table.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn remove(&self, dst_id: i64, pool: &sqlx::PgPool) -> Result<(), ExecError> {
        self.remove_pool(dst_id, &Pool::Postgres(pool.clone()))
            .await
    }

    /// Replace the full set of linked destination PKs with `ids`.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn set(&self, ids: &[i64], pool: &sqlx::PgPool) -> Result<(), ExecError> {
        self.set_pool(ids, &Pool::Postgres(pool.clone())).await
    }

    /// Remove all junction rows for the source instance.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn clear(&self, pool: &sqlx::PgPool) -> Result<(), ExecError> {
        self.clear_pool(&Pool::Postgres(pool.clone())).await
    }

    /// Return `true` if `dst_id` is linked to the source instance.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn contains(&self, dst_id: i64, pool: &sqlx::PgPool) -> Result<bool, ExecError> {
        self.contains_pool(dst_id, &Pool::Postgres(pool.clone()))
            .await
    }
}

// ============================================================ small per-backend helpers

/// Run a SELECT that returns one `i64` column per row and collect the
/// values. Used by `all_pool` + `contains_pool`. Routes through
/// `raw_query_pool` (single-column tuple decode) — #561 collapsed
/// what was a 3-arm `match pool` with byte-identical
/// `try_get::<i64, _>(col_name)` loops.
///
/// The `col_name` argument is no longer consulted (the underlying
/// SELECT must already be single-column, which both callers honor):
/// `raw_query_pool::<(i64,)>` decodes positionally.
async fn fetch_i64_col_pool(
    pool: &Pool,
    sql: &str,
    binds: Vec<SqlValue>,
    _col_name: &str,
) -> Result<Vec<i64>, ExecError> {
    let rows: Vec<(i64,)> = crate::sql::raw_query_pool(sql, binds, pool).await?;
    Ok(rows.into_iter().map(|(v,)| v).collect())
}

// #561 — the three local `bind_pg`/`bind_my`/`bind_sqlite` helpers
// were removed once `set_pool` started routing through
// `raw_execute_tx` (which uses the canonical executor `bind_query*`
// path). The audit-tx + m2m-set body is now a single flat sequence.
