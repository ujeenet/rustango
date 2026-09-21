//! A [`Cache`] backend that stores one row per key in the database.
//! The table is `cache_key`, `value` and `expires`,
//! with the same layout on PG, MySQL and SQLite. Expired rows are
//! removed on read; there is no background reaper.
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
//! `expires` holds Unix **milliseconds**; `0` means the entry never
//! expires. An entry is expired once now is *past* `expires`, not once
//! it reaches it — the same rule as [`super::InMemoryCache`] and
//! [`super::FileCache`]. A `get` or `exists` that lands on an expired
//! row deletes it and reports a miss.
//!
//! ## Why not a migration?
//!
//! The cache table does not follow the app's data. You can drop it,
//! recreate it, or keep it in another database.
//! [`DatabaseCache::ensure_table`] runs the right
//! `CREATE TABLE IF NOT EXISTS` at boot; `manage createcachetable`
//! does the same from the CLI.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::{Cache, CacheError};
use crate::core::SqlValue;
use crate::sql::{raw_execute_pool, raw_query_pool, Pool};

/// Cache stored in a SQL table. Holds a [`Pool`] and a table name;
/// every operation runs one statement written for that dialect.
#[derive(Clone)]
pub struct DatabaseCache {
    pool: Pool,
    table: String,
}

impl DatabaseCache {
    /// Build a cache writing to `table` on `pool`. Call
    /// [`Self::ensure_table`] once at startup to create the table.
    #[must_use]
    pub fn new(pool: Pool, table: impl Into<String>) -> Self {
        Self {
            pool,
            table: table.into(),
        }
    }

    /// The configured table name.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Create the cache table if it is missing. Safe to call at every
    /// boot.
    ///
    /// # Errors
    /// [`CacheError::Connection`] when the DDL fails, for example on a
    /// permission problem.
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
            // SQLite, and a reasonable default for anything else.
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

    /// Drop the cache table. Handy in tests. In production, run it
    /// from a manage verb rather than from app code.
    ///
    /// # Errors
    /// [`CacheError::Connection`] when the statement fails.
    pub async fn drop_table(&self) -> Result<(), CacheError> {
        let table = self.pool.dialect().quote_ident(&self.table);
        let sql = format!("DROP TABLE IF EXISTS {table}");
        raw_execute_pool(&self.pool, &sql, vec![])
            .await
            .map_err(|e| CacheError::Connection(format!("drop_table: {e}")))?;
        Ok(())
    }

    /// Delete every expired row now. `get` and `exists` already clean
    /// up rows they touch, so run this on a schedule to reclaim space
    /// from keys nobody reads.
    ///
    /// Returns the number of rows deleted. Rows with `expires = 0`
    /// have no TTL and are never touched.
    ///
    /// # Errors
    /// [`CacheError::Connection`] when the DELETE fails.
    pub async fn purge_expired(&self) -> Result<u64, CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        // Keep no-TTL rows. Use the same clock as `get`/`set` so both
        // agree on what "expired" means.
        let sql = format!("DELETE FROM {table} WHERE expires != 0 AND expires < {p1}");
        let now = Self::now_unix_ms();
        raw_execute_pool(&self.pool, &sql, vec![SqlValue::I64(now)])
            .await
            .map_err(|e| CacheError::Connection(format!("purge_expired: {e}")))
    }

    /// Unix epoch in **milliseconds**. Seconds are too coarse: a
    /// `set` at `HH:MM:SS.999` read back 2 ms later would truncate to
    /// two different seconds, so a 1-second TTL looked expired at
    /// once.
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
        // `>`, not `>=`, so an entry lives its full stated duration.
        // The other backends and `purge_expired` use `>` as well.
        if expires != 0 && Self::now_unix_ms() > expires {
            // Drop the dead row before reporting a miss.
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

    /// Atomic set-if-absent. `DistributedLock` needs this: the trait
    /// default checks and then writes, so two racers could both get
    /// `Ok(true)` and both run the guarded body.
    ///
    /// An expired row must still be takeable, so this is not a plain
    /// `INSERT`. It takes the row when it is absent *or* expired, and
    /// says whether it did. A row with `expires = 0` never expires, so
    /// a live permanent entry is never stolen.
    ///
    /// The database does the test-and-set under row locks, so exactly
    /// one racer wins: once the winner moves `expires` forward, the
    /// loser's `WHERE` no longer matches.
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        let p2 = dialect.placeholder(2);
        let p3 = dialect.placeholder(3);
        let p4 = dialect.placeholder(4);
        let expires = Self::expires_for(ttl);
        let now = Self::now_unix_ms();

        // Bind args in the order the placeholders appear in the text.
        // MySQL and SQLite use positional `?`. Postgres' `$n` would
        // accept any order, which is how a MySQL-only mismatch hides
        // until a MySQL test runs.
        if dialect.name() == "mysql" {
            // MySQL's `ON DUPLICATE KEY UPDATE` takes no WHERE, so
            // this needs two statements. Still race-free: the INSERT
            // covers the absent case, and the UPDATE's own predicate
            // is the compare-and-swap for the expired one.
            let inserted = raw_execute_pool(
                &self.pool,
                &format!(
                    "INSERT IGNORE INTO {table} (cache_key, value, expires) \
                     VALUES ({p1}, {p2}, {p3})"
                ),
                vec![
                    SqlValue::String(key.to_owned()),
                    SqlValue::String(value.to_owned()),
                    SqlValue::I64(expires),
                ],
            )
            .await
            .map_err(|e| CacheError::Connection(format!("add: {e}")))?;
            if inserted == 1 {
                return Ok(true);
            }
            let took = raw_execute_pool(
                &self.pool,
                &format!(
                    "UPDATE {table} SET value = {p1}, expires = {p2} \
                     WHERE cache_key = {p3} AND expires <> 0 AND expires <= {p4}"
                ),
                vec![
                    SqlValue::String(value.to_owned()),
                    SqlValue::I64(expires),
                    SqlValue::String(key.to_owned()),
                    SqlValue::I64(now),
                ],
            )
            .await
            .map_err(|e| CacheError::Connection(format!("add: {e}")))?;
            return Ok(took == 1);
        }

        // Postgres and SQLite support `ON CONFLICT … DO UPDATE …
        // WHERE`, so one statement does it: insert, or overwrite only
        // an expired row. A failed predicate writes nothing and
        // reports 0 rows.
        let took = raw_execute_pool(
            &self.pool,
            &format!(
                "INSERT INTO {table} (cache_key, value, expires) \
                 VALUES ({p1}, {p2}, {p3}) \
                 ON CONFLICT (cache_key) DO UPDATE \
                 SET value = EXCLUDED.value, expires = EXCLUDED.expires \
                 WHERE {table}.expires <> 0 AND {table}.expires <= {p4}"
            ),
            // Textual placeholder order; see the note above.
            vec![
                SqlValue::String(key.to_owned()),
                SqlValue::String(value.to_owned()),
                SqlValue::I64(expires),
                SqlValue::I64(now),
            ],
        )
        .await
        .map_err(|e| CacheError::Connection(format!("add: {e}")))?;
        Ok(took == 1)
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

    /// Prefix delete with `LIKE 'prefix%'`. The keys are a real
    /// column, so no whole-table clear is needed.
    ///
    /// `%` and `_` are LIKE wildcards, so a prefix containing either
    /// has to be escaped or it would sweep a neighbour's rows.
    ///
    /// The escape character is **`!`, not `\`**. A backslash is not
    /// portable: `ESCAPE '\'` is a syntax error on MySQL but valid on
    /// Postgres. `!` needs no escaping on any of the three, so one
    /// statement works everywhere.
    ///
    /// One thing to watch: `LIKE` uses the column's collation, which
    /// ignores ASCII case on SQLite and, by default, on MySQL, while
    /// `get` and `delete` compare with `=`. Two namespaces that differ
    /// only in case collide on a prefix delete but not on a read. Keep
    /// namespaces lower-case, as tenant slugs already are.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p = dialect.placeholder(1);
        // Use the crate-wide escaper so the escape character cannot
        // drift between the cache and the ORM's LIKE lookups.
        let pattern = format!("{}%", crate::core::escape_like(prefix));
        let sql = format!(
            "DELETE FROM {table} WHERE cache_key LIKE {p}{}",
            crate::core::LIKE_ESCAPE_CLAUSE
        );
        raw_execute_pool(&self.pool, &sql, vec![SqlValue::String(pattern)])
            .await
            .map_err(|e| CacheError::Connection(format!("delete_prefix: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No TTL means `expires = 0`, which never expires.
    #[test]
    fn expires_for_zero_when_no_ttl() {
        assert_eq!(DatabaseCache::expires_for(None), 0);
    }

    #[test]
    fn expires_for_offsets_from_now() {
        // A 60-second TTL adds 60_000 ms to the clock reading.
        let before = DatabaseCache::now_unix_ms();
        let ts = DatabaseCache::expires_for(Some(Duration::from_secs(60)));
        let after = DatabaseCache::now_unix_ms();
        assert!(
            ts >= before + 60_000 && ts <= after + 60_000,
            "expected expires in [{before}+60_000, {after}+60_000], got {ts}"
        );
    }
}
