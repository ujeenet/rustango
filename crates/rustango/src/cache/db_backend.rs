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

    /// Eagerly delete every expired row. Pairs with the implicit
    /// lazy GC on `get` / `exists` — call this from a periodic
    /// cron / scheduled-task / `manage` verb to reclaim space
    /// from keys nobody reads anymore. Django parity for
    /// `manage clearsessions` (when the session backend is the
    /// DB cache) + the broader `manage clearcache` flow.
    ///
    /// Returns the number of rows deleted. Rows with `expires = 0`
    /// (no TTL) are never touched.
    ///
    /// # Errors
    /// [`CacheError::Connection`] forwarded from the executor on
    /// DELETE failure.
    pub async fn purge_expired(&self) -> Result<u64, CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        // Keep `expires = 0` (no-TTL) rows. Compare against the
        // same `now_unix_ms` reading the get/set path uses so this
        // method's notion of "expired" stays consistent with the
        // lazy GC branch.
        let sql = format!("DELETE FROM {table} WHERE expires != 0 AND expires < {p1}");
        let now = Self::now_unix_ms();
        raw_execute_pool(&self.pool, &sql, vec![SqlValue::I64(now)])
            .await
            .map_err(|e| CacheError::Connection(format!("purge_expired: {e}")))
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

    /// Atomic set-if-absent (#1281).
    ///
    /// Without this override `DatabaseCache` inherited the trait default
    /// — `exists()` then `set()` — which is a check-then-act across two
    /// round trips, and `set` is an unconditional upsert, so two racers
    /// both saw "absent", both wrote, and **both got `Ok(true)`**. That
    /// silently voided `DistributedLock`'s entire guarantee on this
    /// backend: two processes ran the same guarded body. Redis and
    /// in-memory already overrode `add`; this was the third backend.
    ///
    /// An expired row must still be acquirable, so this is not a bare
    /// `INSERT` — it takes the row when it is absent *or* already
    /// expired, and reports whether it did. `expires = 0` means "never
    /// expires" (see [`Self::expires_for`]), so a live persistent entry
    /// is never stolen.
    ///
    /// The database does the test-and-set under row locks, so a race
    /// resolves to exactly one winner: the loser's `WHERE` no longer
    /// matches once the winner has pushed `expires` into the future.
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p1 = dialect.placeholder(1);
        let p2 = dialect.placeholder(2);
        let p3 = dialect.placeholder(3);
        let p4 = dialect.placeholder(4);
        let expires = Self::expires_for(ttl);
        let now = Self::now_unix_ms();

        // Bind order is *textual* placeholder order, not logical order:
        // MySQL and SQLite use positional `?`, so the args must appear in
        // the sequence the placeholders do. (Postgres' numbered `$n`
        // would tolerate any order, which is exactly how a MySQL-only
        // mismatch stays hidden until a MySQL test runs it.)
        if dialect.name() == "mysql" {
            // MySQL's `ON DUPLICATE KEY UPDATE` takes no WHERE, so the
            // conditional take is two statements. Both are individually
            // atomic and the pair is still race-free: the INSERT settles
            // the absent case, and for the expired case the UPDATE's own
            // predicate is the compare-and-swap — whoever lands first
            // moves `expires` forward and the other matches nothing.
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

        // Postgres and SQLite both support `ON CONFLICT … DO UPDATE …
        // WHERE`, which expresses the whole thing in one statement:
        // insert, or overwrite only an expired row. When the predicate
        // fails nothing is written and the statement reports 0 rows.
        let took = raw_execute_pool(
            &self.pool,
            &format!(
                "INSERT INTO {table} (cache_key, value, expires) \
                 VALUES ({p1}, {p2}, {p3}) \
                 ON CONFLICT (cache_key) DO UPDATE \
                 SET value = EXCLUDED.value, expires = EXCLUDED.expires \
                 WHERE {table}.expires <> 0 AND {table}.expires <= {p4}"
            ),
            // Textual placeholder order — see the note above.
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

    /// Exact prefix delete via `LIKE 'prefix%'` — the keys are a real
    /// column, so there is no need for the trait default's whole-table
    /// clear (#1227).
    ///
    /// `%` and `_` are LIKE wildcards, so a prefix containing either
    /// would match more rows than intended — an unescaped `_` matches
    /// any single character, and one namespace's clear could sweep a
    /// neighbour's rows.
    ///
    /// The escape character is **`!`, not `\`**. A backslash cannot be
    /// written portably here: MySQL treats `\` as an escape *inside
    /// string literals*, so `ESCAPE '\'` is a syntax error there, while
    /// Postgres (with `standard_conforming_strings`) reads the same
    /// literal as one backslash and accepts it. `!` needs no escaping in
    /// any of the three dialects, so one statement works everywhere.
    /// Covered on all three by `cache_db_backend_sqlite_live.rs` and
    /// `cache_delete_prefix_live.rs`.
    ///
    /// One asymmetry to know about: `LIKE` uses the column's collation,
    /// which folds ASCII case on SQLite and is case-insensitive by
    /// default on MySQL, while `get` / `delete` compare with `=`. Two
    /// namespaces differing only in case therefore collide on a prefix
    /// delete but not on a read. Keep namespaces lower-case — tenant
    /// slugs already are — and it does not arise.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        let dialect = self.pool.dialect();
        let table = dialect.quote_ident(&self.table);
        let p = dialect.placeholder(1);
        // One escape algorithm for the whole crate (#1257 promoted this
        // module's `!`-based escaping to `core::escape_like`); reusing
        // it here means the escape character can never drift between
        // the cache and the ORM's LIKE lookups.
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
