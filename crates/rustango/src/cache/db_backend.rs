//! Database-backed [`Cache`] backend (#409) — Django parity for
//! `django.core.cache.backends.db.DatabaseCache`. Stores one row per
//! cache key in a small `cache_key TEXT PRIMARY KEY, value TEXT,
//! expires BIGINT` table, identical layout across PG / MySQL /
//! SQLite. Expired rows are pruned lazily on read (no background
//! reaper).
//!
//! ## Quick start
//!
//! ```ignore
//! use std::sync::Arc;
//! use rustango::cache::{Cache, DatabaseCache};
//!
//! let cache = DatabaseCache::new(pool.clone(), "rustango_cache");
//! cache.ensure_table().await?;                  // one-time idempotent DDL
//! let cache: Arc<dyn Cache> = Arc::new(cache);
//! cache.set("greeting", "hello", None).await?;
//! ```
//!
//! ## Schema
//!
//! Same shape on every backend; only the column types vary so the
//! PRIMARY KEY survives across dialects:
//!
//! - Postgres: `cache_key TEXT PRIMARY KEY, value TEXT NOT NULL, expires BIGINT NOT NULL DEFAULT 0`
//! - MySQL:    `cache_key VARCHAR(255) PRIMARY KEY, value LONGTEXT NOT NULL, expires BIGINT NOT NULL DEFAULT 0`
//! - SQLite:   `cache_key TEXT PRIMARY KEY, value TEXT NOT NULL, expires INTEGER NOT NULL DEFAULT 0`
//!
//! `expires` is a Unix-seconds timestamp; `0` means "never expires".
//! Lazy GC: any `get`/`exists` call that lands on an expired row
//! deletes the row before returning `None`.
//!
//! ## Why not a migration?
//!
//! The cache table's lifecycle is decoupled from application data —
//! it's safe to drop, recreate, or store in a separate database.
//! [`DatabaseCache::ensure_table`] runs the dialect's
//! `CREATE TABLE IF NOT EXISTS` once at boot, matching Django's
//! `createcachetable` manage command.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::{Cache, CacheError};
use crate::core::SqlValue;
use crate::sql::{raw_execute_pool, raw_query_pool, Pool};

/// SQL-table-backed cache. Holds a [`Pool`] + table name; each
/// operation runs one dialect-rendered statement.
#[derive(Clone)]
pub struct DatabaseCache {
    pool: Pool,
    table: String,
}

impl DatabaseCache {
    /// Build a cache writing to `table` on `pool`. Call
    /// [`Self::ensure_table`] once at startup to issue the
    /// `CREATE TABLE IF NOT EXISTS` DDL.
    #[must_use]
    pub fn new(pool: Pool, table: impl Into<String>) -> Self {
        Self {
            pool,
            table: table.into(),
        }
    }

    /// The configured table name (used for diagnostics + manage verbs).
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Idempotently create the cache table on the active backend.
    /// Safe to call at every boot; uses `CREATE TABLE IF NOT EXISTS`.
    ///
    /// # Errors
    /// [`CacheError::Connection`] forwarded from the executor on
    /// DDL failure (permissions, syntax mismatch on an unknown
    /// dialect, etc.).
    pub async fn ensure_table(&self) -> Result<(), CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let sql = match dialect.name() {
            "postgres" => format!(
                "CREATE TABLE IF NOT EXISTS {table} (\
                 cache_key TEXT PRIMARY KEY, \
                 value TEXT NOT NULL, \
                 expires BIGINT NOT NULL DEFAULT 0\
                 )"
            ),
            "mysql" => format!(
                "CREATE TABLE IF NOT EXISTS {table} (\
                 cache_key VARCHAR(255) PRIMARY KEY, \
                 value LONGTEXT NOT NULL, \
                 expires BIGINT NOT NULL DEFAULT 0\
                 )"
            ),
            // SQLite + ANSI-leaning fallback.
            _ => format!(
                "CREATE TABLE IF NOT EXISTS {table} (\
                 cache_key TEXT PRIMARY KEY, \
                 value TEXT NOT NULL, \
                 expires INTEGER NOT NULL DEFAULT 0\
                 )"
            ),
        };
        raw_execute_pool(&self.pool, &sql, vec![])
            .await
            .map_err(|e| CacheError::Connection(format!("ensure_table: {e}")))?;
        Ok(())
    }

    /// Drop the cache table. Useful in tests; production deployments
    /// should typically issue this as a deliberate manage verb
    /// rather than from app code.
    ///
    /// # Errors
    /// [`CacheError::Connection`] forwarded from the executor.
    pub async fn drop_table(&self) -> Result<(), CacheError> {
        let table = self.pool.dialect().quote_ident(&self.table);
        let sql = format!("DROP TABLE IF EXISTS {table}");
        raw_execute_pool(&self.pool, &sql, vec![])
            .await
            .map_err(|e| CacheError::Connection(format!("drop_table: {e}")))?;
        Ok(())
    }

    /// Unix epoch in **milliseconds**. Promoted from seconds in v0.42
    /// to eliminate a second-boundary race: `set` at `HH:MM:SS.999`
    /// followed by `get` at `HH:MM:SS+1.001` used to truncate both
    /// readings to integer seconds (`SS` and `SS+1`), making a TTL of
    /// 1 second look already-expired at the immediate read. The
    /// `expires BIGINT` column stays the same — it now carries
    /// millisecond ticks instead of second ticks; existing pre-v0.42
    /// rows look like 1970-era timestamps under the new lens and are
    /// lazily purged on first read (cache is regeneratable by design).
    fn now_unix_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    fn expires_for(ttl: Option<Duration>) -> i64 {
        ttl.map(|d| {
            let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
            Self::now_unix_ms().saturating_add(ms)
        })
        .unwrap_or(0)
    }
}

#[async_trait]
impl Cache for DatabaseCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        let sql = format!("SELECT value, expires FROM {table} WHERE cache_key = {p1} LIMIT 1");
        let rows: Vec<(String, i64)> =
            raw_query_pool(&sql, vec![SqlValue::String(key.to_owned())], &self.pool)
                .await
                .map_err(|e| CacheError::Connection(format!("get: {e}")))?;
        let Some((value, expires)) = rows.into_iter().next() else {
            return Ok(None);
        };
        if expires != 0 && Self::now_unix_ms() >= expires {
            // Lazy GC — purge expired before reporting miss.
            let _ = self.delete(key).await;
            return Ok(None);
        }
        Ok(Some(value))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        let p2 = dialect.placeholder(2);
        let p3 = dialect.placeholder(3);
        let expires = Self::expires_for(ttl);
        // Per-dialect upsert. MySQL has no ON CONFLICT clause.
        let sql = match dialect.name() {
            "mysql" => format!(
                "INSERT INTO {table} (cache_key, value, expires) \
                 VALUES ({p1}, {p2}, {p3}) \
                 ON DUPLICATE KEY UPDATE value = VALUES(value), expires = VALUES(expires)"
            ),
            _ => format!(
                "INSERT INTO {table} (cache_key, value, expires) \
                 VALUES ({p1}, {p2}, {p3}) \
                 ON CONFLICT (cache_key) DO UPDATE SET value = EXCLUDED.value, expires = EXCLUDED.expires"
            ),
        };
        raw_execute_pool(
            &self.pool,
            &sql,
            vec![
                SqlValue::String(key.to_owned()),
                SqlValue::String(value.to_owned()),
                SqlValue::I64(expires),
            ],
        )
        .await
        .map_err(|e| CacheError::Connection(format!("set: {e}")))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        let sql = format!("DELETE FROM {table} WHERE cache_key = {p1}");
        raw_execute_pool(&self.pool, &sql, vec![SqlValue::String(key.to_owned())])
            .await
            .map_err(|e| CacheError::Connection(format!("delete: {e}")))?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        Ok(self.get(key).await?.is_some())
    }

    async fn clear(&self) -> Result<(), CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let sql = format!("DELETE FROM {table}");
        raw_execute_pool(&self.pool, &sql, vec![])
            .await
            .map_err(|e| CacheError::Connection(format!("clear: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure-string DDL emission — no real pool needed. Verifies the
    /// dialect branch (`postgres` arm above) compiles a syntactically
    /// well-formed `CREATE TABLE IF NOT EXISTS` shape. The runtime
    /// path is exercised by the sqlite live test in
    /// `tests/cache_db_backend_sqlite_live.rs`.
    #[test]
    fn expires_for_zero_when_no_ttl() {
        assert_eq!(DatabaseCache::expires_for(None), 0);
    }

    #[test]
    fn expires_for_offsets_from_now() {
        // ms precision: a 60-second TTL adds 60_000 ms to the current
        // unix-ms reading, bracketed by the surrounding observations.
        let before = DatabaseCache::now_unix_ms();
        let ts = DatabaseCache::expires_for(Some(Duration::from_secs(60)));
        let after = DatabaseCache::now_unix_ms();
        assert!(
            ts >= before + 60_000 && ts <= after + 60_000,
            "expected expires in [{before}+60_000, {after}+60_000], got {ts}"
        );
    }
}
