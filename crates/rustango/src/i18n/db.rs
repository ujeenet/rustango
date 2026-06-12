//! Editable DB-override layer for translations — issue #532 Slice 1.
//!
//! `Translator::from_directory` loads `locales/<lang>.json` on boot and
//! that's the *only* ingest path: fixing a typo in `fr.json` means an
//! editor + a redeploy. This module adds the hybrid storage layer the
//! issue's Slice 1 calls for — a `rustango_translations` table whose
//! rows override the file catalogs, so operators (eventually via the
//! Slice 2 admin UI) can edit translations without shipping code.
//!
//! Lookup order in [`crate::i18n::Translator::translate`] becomes:
//!
//! 1. DB override (this table, loaded into the in-memory override map
//!    via [`refresh_overrides_pool`])
//! 2. File-loaded JSON catalog (read-only seed, unchanged)
//! 3. Default-locale catalog (existing fallback chain)
//!
//! Files stay the source of truth for shipped defaults;
//! [`seed_from_translator_pool`] copies them into the DB once
//! (`ON CONFLICT DO NOTHING`), and edits live as DB rows on top.
//!
//! The table is created by [`ensure_table_pool`] across all three
//! dialects (mirroring `audit` / `contenttypes`) rather than the
//! migration graph, so it works for any app holding a [`Pool`] —
//! tenancy or not. Tenancy apps create it on the registry pool.

use crate::i18n::Translator;
use crate::sql::{Auto, ExecError, Pool};
use crate::Model;

/// One editable translation, table `rustango_translations`. The
/// `(locale, key)` pair is unique — one editable value per key per
/// locale.
///
/// `managed = false`: created by [`ensure_table_pool`], not the
/// migration graph. The table also carries DB-defaulted `created_at` /
/// `updated_at` columns for the Slice 2 audit trail; they're not mapped
/// here because the ORM only `SELECT`s declared columns, so the extra
/// columns are harmless to this model.
#[derive(Model, Debug, Clone, serde::Serialize)]
#[rustango(table = "rustango_translations", managed = false)]
pub struct Translation {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub locale: String,
    #[rustango(max_length = 200)]
    pub key: String,
    pub value: String,
    #[rustango(max_length = 200, default = "")]
    pub updated_by: String,
}

const DDL_PG: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_translations" (
    "id"         BIGSERIAL PRIMARY KEY,
    "locale"     VARCHAR(16) NOT NULL,
    "key"        VARCHAR(200) NOT NULL,
    "value"      TEXT NOT NULL,
    "updated_by" VARCHAR(200) NOT NULL DEFAULT '',
    "created_at" TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    "updated_at" TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT "rustango_translations_locale_key_uq" UNIQUE ("locale", "key")
);
"#;

const DDL_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_translations" (
    "id"         INTEGER PRIMARY KEY AUTOINCREMENT,
    "locale"     TEXT NOT NULL,
    "key"        TEXT NOT NULL,
    "value"      TEXT NOT NULL,
    "updated_by" TEXT NOT NULL DEFAULT '',
    "created_at" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "rustango_translations_locale_key_uq" UNIQUE ("locale", "key")
);
"#;

const DDL_MYSQL: &str = r"
CREATE TABLE IF NOT EXISTS `rustango_translations` (
    `id`         BIGINT AUTO_INCREMENT PRIMARY KEY,
    `locale`     VARCHAR(16) NOT NULL,
    `key`        VARCHAR(200) NOT NULL,
    `value`      TEXT NOT NULL,
    `updated_by` VARCHAR(200) NOT NULL DEFAULT '',
    `created_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    `updated_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT `rustango_translations_locale_key_uq` UNIQUE (`locale`, `key`)
);
";

/// Create `rustango_translations` if absent — idempotent, dispatched per
/// dialect. Call once at startup (and the Slice 2 `seed-translations`
/// verb calls it too).
///
/// # Errors
/// Driver / SQL failures other than the duplicate-object errors that
/// [`crate::sql::run_ddl_idempotent`] swallows.
pub async fn ensure_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "mysql" => DDL_MYSQL,
        "sqlite" => DDL_SQLITE,
        // Postgres + any future dialect: the standard `"`-quoted DDL.
        _ => DDL_PG,
    };
    crate::sql::run_ddl_idempotent(pool, ddl).await
}

/// Every override row, for the admin list view / [`refresh_overrides_pool`].
///
/// # Errors
/// As the ORM fetch path ([`ExecError`]).
pub async fn all_pool(pool: &Pool) -> Result<Vec<Translation>, ExecError> {
    use crate::sql::FetcherPool as _;
    Translation::objects().fetch_pool(pool).await
}

/// Insert-or-update the editable value for `(locale, key)` — the
/// "edit without redeploy" write. `updated_by` records who made the
/// change (operator id / username); pass `""` if not tracked.
///
/// # Errors
/// As the ORM upsert path ([`ExecError`]).
pub async fn upsert_pool(
    pool: &Pool,
    locale: &str,
    key: &str,
    value: &str,
    updated_by: &str,
) -> Result<(), ExecError> {
    let row = Translation {
        id: Auto::Unset,
        locale: locale.to_owned(),
        key: key.to_owned(),
        value: value.to_owned(),
        updated_by: updated_by.to_owned(),
    };
    Translation::bulk_upsert_pool(&[row], &["locale", "key"], &["value", "updated_by"], pool).await
}

/// Seed the DB layer from a [`Translator`]'s file catalogs. Existing
/// `(locale, key)` rows are left untouched (`ON CONFLICT DO NOTHING`),
/// so files stay the source of truth for shipped defaults and only
/// previously-unknown keys are added. Idempotent — safe to run on every
/// boot. Returns the number of file entries processed.
///
/// The on-disk path is the caller's: load with
/// `Translator::from_directory(dir, default)?` then hand the result here
/// (keeps the filesystem error out of [`ExecError`]).
///
/// # Errors
/// As the ORM bulk-insert path ([`ExecError`]).
pub async fn seed_from_translator_pool(
    pool: &Pool,
    translator: &Translator,
) -> Result<usize, ExecError> {
    let rows: Vec<Translation> = translator
        .entries()
        .into_iter()
        .map(|(locale, key, value)| Translation {
            id: Auto::Unset,
            locale,
            key,
            value,
            updated_by: String::new(),
        })
        .collect();
    let n = rows.len();
    if n > 0 {
        Translation::bulk_insert_or_ignore_pool(&rows, pool).await?;
    }
    Ok(n)
}

/// Reload `translator`'s override layer from the DB so subsequent
/// `translate(...)` calls reflect persisted edits — call after seeding,
/// on boot, and after each admin save. Returns the row count loaded.
///
/// # Errors
/// As the ORM fetch path ([`ExecError`]).
pub async fn refresh_overrides_pool(
    translator: &Translator,
    pool: &Pool,
) -> Result<usize, ExecError> {
    let rows = all_pool(pool).await?;
    let n = rows.len();
    translator.load_overrides(rows.into_iter().map(|t| (t.locale, t.key, t.value)));
    Ok(n)
}
