//! `MediaTag` — flat, free-form labels on [`Media`] rows.
//!
//! Sibling to [`crate::media::collection::MediaCollection`]: tags
//! express inclusive labels ("featured", "approved",
//! "homepage-hero"); collections express exclusive location.
//! M2M between Media and Tag via `rustango_media_tag_links`.
//!
//! Tags are cheap to recreate, so deletion is hard (not soft) — the
//! junction rows cascade away with the FK.
//!
//! v0.38 — tri-dialect via [`Self::ensure_table_pool`] + per-backend
//! [`Self::from_row`] dispatch. The PG-only `ensure_table(&PgPool)`
//! stays as a documented back-compat shim.
//!
//! [`Media`]: crate::media::Media

use chrono::{DateTime, Utc};
#[cfg(feature = "postgres")]
use sqlx::PgPool;

use crate::sql::Auto;

#[derive(Debug, Clone)]
pub struct MediaTag {
    pub id: Auto<i64>,
    pub name: String,
    /// Path-friendly id, unique across the table.
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

const CREATE_TABLE_SQL_PG: &str = "\
CREATE TABLE IF NOT EXISTS rustango_media_tags (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT        NOT NULL,
    slug       TEXT        NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE IF NOT EXISTS rustango_media_tag_links (
    media_id BIGINT NOT NULL,
    tag_id   BIGINT NOT NULL,
    PRIMARY KEY (media_id, tag_id),
    FOREIGN KEY (tag_id)
        REFERENCES rustango_media_tags (id)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS rustango_media_tag_links_tag_idx
    ON rustango_media_tag_links (tag_id)";

const CREATE_TABLE_SQL_MYSQL: &str = "\
CREATE TABLE IF NOT EXISTS `rustango_media_tags` (
    `id`         BIGINT      NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `name`       VARCHAR(255) NOT NULL,
    `slug`       VARCHAR(255) NOT NULL UNIQUE,
    `created_at` DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
CREATE TABLE IF NOT EXISTS `rustango_media_tag_links` (
    `media_id` BIGINT NOT NULL,
    `tag_id`   BIGINT NOT NULL,
    PRIMARY KEY (`media_id`, `tag_id`),
    FOREIGN KEY (`tag_id`)
        REFERENCES `rustango_media_tags` (`id`)
        ON DELETE CASCADE
);
CREATE INDEX `rustango_media_tag_links_tag_idx`
    ON `rustango_media_tag_links` (`tag_id`)";

const CREATE_TABLE_SQL_SQLITE: &str = "\
CREATE TABLE IF NOT EXISTS rustango_media_tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT     NOT NULL,
    slug       TEXT     NOT NULL UNIQUE,
    created_at TEXT     NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TABLE IF NOT EXISTS rustango_media_tag_links (
    media_id INTEGER NOT NULL,
    tag_id   INTEGER NOT NULL,
    PRIMARY KEY (media_id, tag_id),
    FOREIGN KEY (tag_id)
        REFERENCES rustango_media_tags (id)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS rustango_media_tag_links_tag_idx
    ON rustango_media_tag_links (tag_id)";

impl MediaTag {
    /// PG back-compat shim around [`Self::ensure_table_pool`].
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    #[cfg(feature = "postgres")]
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        Self::ensure_table_pool(&crate::sql::Pool::Postgres(pool.clone())).await
    }

    /// Create the `rustango_media_tags` + `rustango_media_tag_links`
    /// junction tables if absent. Idempotent. v0.38 — tri-dialect:
    /// dispatches on `pool.dialect()` for PG / MySQL / SQLite DDL.
    /// MySQL's `CREATE INDEX` raises 1061 if the index already
    /// exists (no `IF NOT EXISTS` clause); that one error is swallowed
    /// for idempotency, every other error surfaces.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table_pool(pool: &crate::sql::Pool) -> Result<(), sqlx::Error> {
        let ddl = match pool.dialect().name() {
            "postgres" => CREATE_TABLE_SQL_PG,
            "mysql" => CREATE_TABLE_SQL_MYSQL,
            "sqlite" => CREATE_TABLE_SQL_SQLITE,
            _ => CREATE_TABLE_SQL_PG,
        };
        // #561 — shared split-+-dispatch-+-swallow-dup-index loop.
        crate::sql::run_ddl_idempotent(pool, ddl).await
    }

    /// PG row decoder — kept for in-crate callers that still acquire
    /// a `PgRow` directly. Tri-dialect callers can use the
    /// `sqlx::FromRow` trait impl below + `raw_query_pool::<MediaTag>`.
    #[cfg(feature = "postgres")]
    pub(super) fn decode_pg(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        Ok(Self {
            id: Auto::Set(id),
            name: row.try_get("name")?,
            slug: row.try_get("slug")?,
            created_at: row.try_get("created_at")?,
        })
    }

    /// MySQL row decoder.
    #[cfg(feature = "mysql")]
    pub(super) fn decode_my(row: &sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        let created_at: DateTime<Utc> = decode_my_datetime(row, "created_at")?;
        Ok(Self {
            id: Auto::Set(id),
            name: row.try_get("name")?,
            slug: row.try_get("slug")?,
            created_at,
        })
    }

    /// SQLite row decoder.
    #[cfg(feature = "sqlite")]
    pub(super) fn decode_sq(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let id: i64 = row.try_get("id")?;
        let created_at: DateTime<Utc> = decode_sqlite_datetime(row, "created_at")?;
        Ok(Self {
            id: Auto::Set(id),
            name: row.try_get("name")?,
            slug: row.try_get("slug")?,
            created_at,
        })
    }
}

// v0.38 — FromRow impls dispatch per backend so `raw_query_pool::<MediaTag>`
// works on every backend.
#[cfg(feature = "postgres")]
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for MediaTag {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Self::decode_pg(row)
    }
}
#[cfg(feature = "mysql")]
impl<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> for MediaTag {
    fn from_row(row: &'r sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        Self::decode_my(row)
    }
}
#[cfg(feature = "sqlite")]
impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for MediaTag {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Self::decode_sq(row)
    }
}

// #561 — the pub(super) `is_mysql_dup_index_error` re-export here
// was a shim for media::mod + media::collection that reached
// across modules to dodge a 5th copy of the predicate. Both of
// those callers now route through `crate::sql::run_ddl_idempotent`
// (the shared DDL runner already swallows MySQL's dup-index
// error), so the re-export has no remaining consumers.

/// MySQL DATETIME(6) decoder — sqlx returns `NaiveDateTime` by default
/// for `DATETIME` (no TZ); promote to `DateTime<Utc>` assuming the
/// column is stored in UTC (matches the DEFAULT CURRENT_TIMESTAMP and
/// every framework-side write).
#[cfg(feature = "mysql")]
pub(super) fn decode_my_datetime(
    row: &sqlx::mysql::MySqlRow,
    col: &str,
) -> Result<DateTime<Utc>, sqlx::Error> {
    use chrono::TimeZone as _;
    use sqlx::Row;
    let naive: chrono::NaiveDateTime = row.try_get(col)?;
    Ok(Utc.from_utc_datetime(&naive))
}

/// MySQL nullable DATETIME(6) decoder.
#[cfg(feature = "mysql")]
pub(super) fn decode_my_datetime_opt(
    row: &sqlx::mysql::MySqlRow,
    col: &str,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    use chrono::TimeZone as _;
    use sqlx::Row;
    let naive: Option<chrono::NaiveDateTime> = row.try_get(col)?;
    Ok(naive.map(|n| Utc.from_utc_datetime(&n)))
}

/// SQLite TEXT-as-ISO-8601 decoder.
#[cfg(feature = "sqlite")]
pub(super) fn decode_sqlite_datetime(
    row: &sqlx::sqlite::SqliteRow,
    col: &str,
) -> Result<DateTime<Utc>, sqlx::Error> {
    use sqlx::Row;
    let s: String = row.try_get(col)?;
    parse_sqlite_dt(&s)
}

/// SQLite nullable TEXT-as-ISO-8601 decoder.
#[cfg(feature = "sqlite")]
pub(super) fn decode_sqlite_datetime_opt(
    row: &sqlx::sqlite::SqliteRow,
    col: &str,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    use sqlx::Row;
    let s: Option<String> = row.try_get(col)?;
    s.map(|s| parse_sqlite_dt(&s)).transpose()
}

#[cfg(feature = "sqlite")]
fn parse_sqlite_dt(s: &str) -> Result<DateTime<Utc>, sqlx::Error> {
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| {
            // SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')` emits
            // `2026-05-12T14:30:42.123Z` which RFC3339 accepts. Some
            // older writes used the bare `YYYY-MM-DD HH:MM:SS` form;
            // try that as a fallback.
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .map(|n| n.and_utc().fixed_offset())
        })
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| sqlx::Error::Decode(format!("sqlite datetime parse: {e} (got `{s}`)").into()))
}
