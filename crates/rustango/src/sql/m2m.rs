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

use super::error::ExecError;
use crate::core::SqlValue;
use sqlx::{PgPool, Row};

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
    ///
    /// # Errors
    /// Driver failures.
    pub async fn all(&self, pool: &PgPool) -> Result<Vec<i64>, ExecError> {
        let sql = format!(
            r#"SELECT "{dst}" FROM "{through}" WHERE "{src}" = $1"#,
            through = self.through,
            src = self.src_col,
            dst = self.dst_col,
        );
        let rows = sqlx::query(&sql)
            .bind(self.src_pk_i64())
            .fetch_all(pool)
            .await
            .map_err(ExecError::Driver)?;
        rows.iter()
            .map(|r| r.try_get::<i64, _>(self.dst_col).map_err(ExecError::Driver))
            .collect()
    }

    /// Add `dst_id` to the junction table. No-op if already present
    /// (`ON CONFLICT DO NOTHING`).
    ///
    /// # Errors
    /// Driver failures.
    pub async fn add(&self, dst_id: i64, pool: &PgPool) -> Result<(), ExecError> {
        let sql = format!(
            r#"INSERT INTO "{through}" ("{src}", "{dst}") VALUES ($1, $2) ON CONFLICT DO NOTHING"#,
            through = self.through,
            src = self.src_col,
            dst = self.dst_col,
        );
        sqlx::query(&sql)
            .bind(self.src_pk_i64())
            .bind(dst_id)
            .execute(pool)
            .await
            .map_err(ExecError::Driver)?;
        Ok(())
    }

    /// Remove `dst_id` from the junction table. No-op if not present.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn remove(&self, dst_id: i64, pool: &PgPool) -> Result<(), ExecError> {
        let sql = format!(
            r#"DELETE FROM "{through}" WHERE "{src}" = $1 AND "{dst}" = $2"#,
            through = self.through,
            src = self.src_col,
            dst = self.dst_col,
        );
        sqlx::query(&sql)
            .bind(self.src_pk_i64())
            .bind(dst_id)
            .execute(pool)
            .await
            .map_err(ExecError::Driver)?;
        Ok(())
    }

    /// Replace the full set of linked destination PKs with `ids`.
    ///
    /// Executes in a single transaction: deletes all existing rows for the
    /// source PK, then bulk-inserts the new set.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn set(&self, ids: &[i64], pool: &PgPool) -> Result<(), ExecError> {
        let mut tx = pool.begin().await.map_err(ExecError::Driver)?;
        let del = format!(
            r#"DELETE FROM "{through}" WHERE "{src}" = $1"#,
            through = self.through,
            src = self.src_col,
        );
        sqlx::query(&del)
            .bind(self.src_pk_i64())
            .execute(&mut *tx)
            .await
            .map_err(ExecError::Driver)?;
        for dst_id in ids {
            let ins = format!(
                r#"INSERT INTO "{through}" ("{src}", "{dst}") VALUES ($1, $2)"#,
                through = self.through,
                src = self.src_col,
                dst = self.dst_col,
            );
            sqlx::query(&ins)
                .bind(self.src_pk_i64())
                .bind(dst_id)
                .execute(&mut *tx)
                .await
                .map_err(ExecError::Driver)?;
        }
        tx.commit().await.map_err(ExecError::Driver)?;
        Ok(())
    }

    /// Remove all junction rows for the source instance.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn clear(&self, pool: &PgPool) -> Result<(), ExecError> {
        let sql = format!(
            r#"DELETE FROM "{through}" WHERE "{src}" = $1"#,
            through = self.through,
            src = self.src_col,
        );
        sqlx::query(&sql)
            .bind(self.src_pk_i64())
            .execute(pool)
            .await
            .map_err(ExecError::Driver)?;
        Ok(())
    }

    /// Return `true` if `dst_id` is linked to the source instance.
    ///
    /// # Errors
    /// Driver failures.
    pub async fn contains(&self, dst_id: i64, pool: &PgPool) -> Result<bool, ExecError> {
        let sql = format!(
            r#"SELECT 1 FROM "{through}" WHERE "{src}" = $1 AND "{dst}" = $2 LIMIT 1"#,
            through = self.through,
            src = self.src_col,
            dst = self.dst_col,
        );
        let row = sqlx::query(&sql)
            .bind(self.src_pk_i64())
            .bind(dst_id)
            .fetch_optional(pool)
            .await
            .map_err(ExecError::Driver)?;
        Ok(row.is_some())
    }

    fn src_pk_i64(&self) -> i64 {
        match &self.src_pk {
            SqlValue::I64(v) => *v,
            SqlValue::I32(v) => i64::from(*v),
            _ => 0,
        }
    }
}
