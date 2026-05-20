//! `manage inspectdb` — emit `#[derive(Model)]` source for every
//! table in a live database. Mirrors Django's `inspectdb` shape:
//! point at an existing schema, get a copy-paste-ready Rust file,
//! adopt rustango against the existing data without rewriting it.
//!
//! v0.38 — tri-dialect. The introspection layer dispatches per
//! backend:
//!   - **PostgreSQL**: ANSI `information_schema.{tables, columns,
//!     table_constraints, key_column_usage, constraint_column_usage}`.
//!     `--schema` filters to a specific PG schema; default `public`.
//!   - **MySQL 8+**: same `information_schema` shape, but
//!     `--schema` maps to MySQL's `TABLE_SCHEMA` (i.e. the database
//!     name). When unset, the current database is used. Auto-
//!     increment detection uses `EXTRA = 'auto_increment'`.
//!   - **SQLite**: `sqlite_master` for the table list,
//!     `PRAGMA table_info(<t>)` for columns,
//!     `PRAGMA foreign_key_list(<t>)` for FKs. `--schema` is ignored
//!     (sqlite has a single namespace per file).
//!
//! ## What it covers
//!
//! - Tables (`BASE TABLE` on PG/MySQL; `type = 'table'` on SQLite)
//! - Views — emitted with `#[rustango(view)]` so the migration runner
//!   skips `CREATE TABLE` / `DROP TABLE` against them. The view's
//!   `view_definition` lands in a fenced ```sql doc comment above the
//!   emitted struct. Issue #293 / T2.10.
//! - Standard column types per backend (integers, text/varchar,
//!   bool, timestamp[tz], date, uuid/blob, json[b]/text-as-json,
//!   numeric/decimal)
//! - PRIMARY KEY → `#[rustango(primary_key)]`
//! - NOT NULL → required field; nullable → `Option<T>`
//! - `varchar(N)` → `#[rustango(max_length = N)]`
//! - SERIAL / IDENTITY / AUTO_INCREMENT / INTEGER PRIMARY KEY
//!   AUTOINCREMENT → `Auto<T>` PK wrapper
//! - FK references → `#[rustango(fk = "<target_table>")]`
//!
//! ## What it skips (v1)
//!
//! - Materialized views, foreign tables (regular views are walked)
//! - Composite primary keys (emits the first PK column with the
//!   marker; the user adjusts as needed)
//! - CHECK constraints (no rustango-side equivalent yet)
//! - Triggers, sequences, generated columns
//! - Custom enum types (mapped to `String` with a `// TODO`)
//! - Index definitions (run `manage makemigrations` after to capture)
//!
//! ## Usage
//!
//! ```sh
//! cargo run -- inspectdb
//! cargo run -- inspectdb --table users
//! cargo run -- inspectdb --schema reporting          # PG / MySQL only
//! cargo run -- inspectdb > src/legacy/models.rs
//! ```

use std::io::Write;

use crate::sql::Pool;

use super::error::MigrateError;

/// Parsed args for the `inspectdb` verb.
#[derive(Debug, Default)]
pub(super) struct InspectdbArgs {
    /// Schema name to scan. PG default `"public"`. On MySQL it's the
    /// database name (defaults to the connection's current DB when
    /// unset). Ignored on SQLite.
    pub schema: String,
    /// When `Some(name)`, only emit that one table.
    pub table: Option<String>,
}

/// Parse `[--schema <name>] [--table <name>]` into `InspectdbArgs`.
pub(super) fn parse_inspectdb_args(args: &[String]) -> Result<InspectdbArgs, MigrateError> {
    let mut out = InspectdbArgs {
        schema: "public".to_owned(),
        table: None,
    };
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--schema" => {
                let v = iter.next().ok_or_else(|| {
                    MigrateError::Validation("`--schema` requires a value".into())
                })?;
                out.schema = v.clone();
            }
            "--table" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("`--table` requires a value".into()))?;
                out.table = Some(v.clone());
            }
            "--help" | "-h" => {
                return Err(MigrateError::Validation(
                    "USAGE: manage inspectdb [--schema <name>] [--table <name>]".into(),
                ));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unknown flag `{other}` for inspectdb (try --help)"
                )));
            }
        }
    }
    Ok(out)
}

/// Top-level entry — read the live schema, emit Rust source. v0.38 —
/// tri-dialect via [`crate::sql::Pool`]. Issue #293 / T2.10 —
/// walks views as well as tables; view-backed models are emitted with
/// `#[rustango(view)]` so the migration runner skips them.
pub(super) async fn inspectdb_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_inspectdb_args(args)?;
    let dialect_name = pool.dialect().name();
    let tables = list_tables(pool, &parsed.schema, parsed.table.as_deref()).await?;
    let views = list_views(pool, &parsed.schema, parsed.table.as_deref()).await?;
    if tables.is_empty() && views.is_empty() {
        let label = if dialect_name == "sqlite" {
            "<database file>".to_owned()
        } else {
            parsed.schema.clone()
        };
        writeln!(
            w,
            "// no tables or views found in `{}`",
            escape_for_comment(&label)
        )?;
        return Ok(());
    }
    write_header(w, &parsed.schema, dialect_name)?;
    for table in &tables {
        let columns = list_columns(pool, &parsed.schema, table).await?;
        let pk_columns = list_pk_columns(pool, &parsed.schema, table).await?;
        let fks = list_fks(pool, &parsed.schema, table).await?;
        write_model_with_dialect(w, table, &columns, &pk_columns, &fks, dialect_name)?;
        writeln!(w)?;
    }
    for view in &views {
        let columns = list_columns(pool, &parsed.schema, view).await?;
        let definition = view_definition(pool, &parsed.schema, view).await?;
        write_view_model_with_dialect(w, view, &columns, definition.as_deref(), dialect_name)?;
        writeln!(w)?;
    }
    Ok(())
}

fn write_header<W: Write>(w: &mut W, schema: &str, dialect: &str) -> std::io::Result<()> {
    let schema_label = if dialect == "sqlite" {
        "<sqlite file>".to_owned()
    } else {
        schema.to_owned()
    };
    writeln!(
        w,
        "//! Auto-emitted by `manage inspectdb` — review before committing.\n\
         //!\n\
         //! Source: `{}` dialect `{}`\n\
         //!\n\
         //! Edits you may need to make:\n\
         //! - Composite primary keys aren't supported by `#[derive(Model)]`;\n\
         //!   inspectdb picks the first PK column. Tighten as needed.\n\
         //! - Custom enum types map to `String` — wrap in a typed enum +\n\
         //!   `From<String>` if you want compile-time safety.\n\
         //! - CHECK constraints, triggers, generated columns, and indexes\n\
         //!   aren't reflected. After editing, run `manage makemigrations`\n\
         //!   to lock in the rest.\n",
        escape_for_comment(&schema_label),
        dialect,
    )?;
    writeln!(w, "use rustango::sql::Auto;")?;
    writeln!(w, "use rustango::Model;\n")?;
    Ok(())
}

/// One column row. The struct is dialect-agnostic — each backend's
/// introspection path normalizes its native column metadata into
/// these fields.
#[derive(Debug, Clone)]
pub(super) struct ColumnRow {
    pub name: String,
    /// Normalized native type token — for PG this is `udt_name`
    /// (e.g. `int8`, `varchar`, `timestamptz`); for MySQL it's
    /// `data_type` (e.g. `bigint`, `varchar`, `datetime`, `json`);
    /// for SQLite the declared type (e.g. `INTEGER`, `TEXT`, `REAL`)
    /// — sqlite's type affinity rules mean a column declared
    /// `VARCHAR(80)` returns `VARCHAR(80)` verbatim, so callers
    /// should match case-insensitively + strip parens.
    pub udt_name: String,
    pub nullable: bool,
    pub max_length: Option<i32>,
    pub default: Option<String>,
    /// `true` when the column is auto-incremented. Detected via
    /// per-dialect signals (PG: `is_identity = YES` or `nextval(...)`;
    /// MySQL: `EXTRA = 'auto_increment'`; SQLite: `INTEGER PRIMARY KEY`
    /// or explicit `AUTOINCREMENT`).
    pub is_auto: bool,
}

// ====================================================================
// Per-dialect introspection
// ====================================================================

#[cfg_attr(
    not(any(feature = "postgres", feature = "mysql")),
    allow(unused_variables)
)]
async fn list_tables(
    pool: &Pool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => list_tables_pg(pg, schema, only).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => list_tables_my(my, schema, only).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => list_tables_sqlite(sq, only).await,
    }
}

#[cfg_attr(
    not(any(feature = "postgres", feature = "mysql")),
    allow(unused_variables)
)]
async fn list_columns(
    pool: &Pool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnRow>, MigrateError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => list_columns_pg(pg, schema, table).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => list_columns_my(my, schema, table).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => list_columns_sqlite(sq, table).await,
    }
}

#[cfg_attr(
    not(any(feature = "postgres", feature = "mysql")),
    allow(unused_variables)
)]
async fn list_pk_columns(
    pool: &Pool,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, MigrateError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => list_pk_columns_pg(pg, schema, table).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => list_pk_columns_my(my, schema, table).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => list_pk_columns_sqlite(sq, table).await,
    }
}

#[cfg_attr(
    not(any(feature = "postgres", feature = "mysql")),
    allow(unused_variables)
)]
async fn list_fks(
    pool: &Pool,
    schema: &str,
    table: &str,
) -> Result<Vec<(String, String)>, MigrateError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => list_fks_pg(pg, schema, table).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => list_fks_my(my, schema, table).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => list_fks_sqlite(sq, table).await,
    }
}

#[cfg_attr(
    not(any(feature = "postgres", feature = "mysql")),
    allow(unused_variables)
)]
async fn list_views(
    pool: &Pool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => list_views_pg(pg, schema, only).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => list_views_my(my, schema, only).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => list_views_sqlite(sq, only).await,
    }
}

#[cfg_attr(
    not(any(feature = "postgres", feature = "mysql")),
    allow(unused_variables)
)]
async fn view_definition(
    pool: &Pool,
    schema: &str,
    view: &str,
) -> Result<Option<String>, MigrateError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => view_definition_pg(pg, schema, view).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => view_definition_my(my, schema, view).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => view_definition_sqlite(sq, view).await,
    }
}

// -------------------------------------------------------------- Postgres

#[cfg(feature = "postgres")]
async fn list_tables_pg(
    pool: &sqlx::PgPool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    let sql = if only.is_some() {
        r#"SELECT table_name FROM information_schema.tables
           WHERE table_schema = $1 AND table_type = 'BASE TABLE' AND table_name = $2
           ORDER BY table_name"#
    } else {
        r#"SELECT table_name FROM information_schema.tables
           WHERE table_schema = $1 AND table_type = 'BASE TABLE'
           ORDER BY table_name"#
    };
    let mut q = sqlx::query(sql).bind(schema);
    if let Some(t) = only {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("table_name").unwrap_or_default())
        .collect())
}

#[cfg(feature = "postgres")]
async fn list_columns_pg(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnRow>, MigrateError> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        r#"SELECT column_name, udt_name, is_nullable, character_maximum_length, column_default,
                  COALESCE(is_identity, 'NO') AS is_identity
           FROM information_schema.columns
           WHERE table_schema = $1 AND table_name = $2
           ORDER BY ordinal_position"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(MigrateError::Driver)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.try_get("column_name").unwrap_or_default();
        let udt_name: String = r.try_get("udt_name").unwrap_or_default();
        let nullable: String = r.try_get("is_nullable").unwrap_or_else(|_| "NO".into());
        let max_length: Option<i32> = r.try_get("character_maximum_length").ok().flatten();
        let default: Option<String> = r.try_get("column_default").ok().flatten();
        let is_identity: String = r.try_get("is_identity").unwrap_or_else(|_| "NO".into());
        let is_auto = is_identity == "YES"
            || default
                .as_deref()
                .is_some_and(|d| d.starts_with("nextval("));
        out.push(ColumnRow {
            name,
            udt_name,
            nullable: nullable == "YES",
            max_length,
            default,
            is_auto,
        });
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn list_pk_columns_pg(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        r#"SELECT kcu.column_name
           FROM information_schema.table_constraints tc
           JOIN information_schema.key_column_usage kcu
             ON tc.constraint_name = kcu.constraint_name
            AND tc.table_schema = kcu.table_schema
           WHERE tc.constraint_type = 'PRIMARY KEY'
             AND tc.table_schema = $1 AND tc.table_name = $2
           ORDER BY kcu.ordinal_position"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("column_name").unwrap_or_default())
        .collect())
}

#[cfg(feature = "postgres")]
async fn list_fks_pg(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<(String, String)>, MigrateError> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        r#"SELECT kcu.column_name AS local_column, ccu.table_name AS foreign_table
           FROM information_schema.table_constraints tc
           JOIN information_schema.key_column_usage kcu
             ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema
           JOIN information_schema.constraint_column_usage ccu
             ON ccu.constraint_name = tc.constraint_name AND ccu.table_schema = tc.table_schema
           WHERE tc.constraint_type = 'FOREIGN KEY'
             AND tc.table_schema = $1 AND tc.table_name = $2
           ORDER BY kcu.ordinal_position"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.try_get::<String, _>("local_column").unwrap_or_default(),
                r.try_get::<String, _>("foreign_table").unwrap_or_default(),
            )
        })
        .collect())
}

#[cfg(feature = "postgres")]
async fn list_views_pg(
    pool: &sqlx::PgPool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    let sql = if only.is_some() {
        r#"SELECT table_name FROM information_schema.views
           WHERE table_schema = $1 AND table_name = $2
           ORDER BY table_name"#
    } else {
        r#"SELECT table_name FROM information_schema.views
           WHERE table_schema = $1
           ORDER BY table_name"#
    };
    let mut q = sqlx::query(sql).bind(schema);
    if let Some(v) = only {
        q = q.bind(v);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("table_name").unwrap_or_default())
        .collect())
}

#[cfg(feature = "postgres")]
async fn view_definition_pg(
    pool: &sqlx::PgPool,
    schema: &str,
    view: &str,
) -> Result<Option<String>, MigrateError> {
    let def: Option<String> = sqlx::query_scalar(
        r#"SELECT view_definition FROM information_schema.views
           WHERE table_schema = $1 AND table_name = $2"#,
    )
    .bind(schema)
    .bind(view)
    .fetch_optional(pool)
    .await
    .map_err(MigrateError::Driver)?;
    Ok(def)
}

// -------------------------------------------------------------- MySQL

#[cfg(feature = "mysql")]
async fn list_tables_my(
    pool: &sqlx::MySqlPool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    // On MySQL, `--schema` is the database name. Default ("public")
    // doesn't make sense — use DATABASE() when no override.
    // MySQL 8.0+ flags `information_schema` string columns as
    // VARBINARY (with `BINARY` collation), which sqlx can't decode as
    // `String` directly — it errors with a `VARCHAR vs VARBINARY`
    // type mismatch. `CAST(... AS CHAR)` forces a real VARCHAR shape
    // that sqlx decodes cleanly. Without this, inspectdb on MySQL
    // silently produced `pub struct Unnamed {}` for every table.
    let use_default = schema == "public" || schema.is_empty();
    let sql = match (only, use_default) {
        (Some(_), true) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.tables
               WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' AND TABLE_NAME = ?
               ORDER BY TABLE_NAME"#
        }
        (Some(_), false) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.tables
               WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' AND TABLE_NAME = ?
               ORDER BY TABLE_NAME"#
        }
        (None, true) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.tables
               WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'
               ORDER BY TABLE_NAME"#
        }
        (None, false) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.tables
               WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE'
               ORDER BY TABLE_NAME"#
        }
    };
    let mut q = sqlx::query(sql);
    if !use_default {
        q = q.bind(schema);
    }
    if let Some(t) = only {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("TABLE_NAME").unwrap_or_default())
        .collect())
}

#[cfg(feature = "mysql")]
async fn list_columns_my(
    pool: &sqlx::MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnRow>, MigrateError> {
    use sqlx::Row as _;
    // See `list_tables_my` for why every string column is `CAST AS CHAR`.
    let use_default = schema == "public" || schema.is_empty();
    let sql = if use_default {
        r#"SELECT CAST(COLUMN_NAME AS CHAR)    AS COLUMN_NAME,
                  CAST(DATA_TYPE AS CHAR)      AS DATA_TYPE,
                  CAST(IS_NULLABLE AS CHAR)    AS IS_NULLABLE,
                  CHARACTER_MAXIMUM_LENGTH,
                  CAST(COLUMN_DEFAULT AS CHAR) AS COLUMN_DEFAULT,
                  CAST(EXTRA AS CHAR)          AS EXTRA
           FROM information_schema.columns
           WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?
           ORDER BY ORDINAL_POSITION"#
    } else {
        r#"SELECT CAST(COLUMN_NAME AS CHAR)    AS COLUMN_NAME,
                  CAST(DATA_TYPE AS CHAR)      AS DATA_TYPE,
                  CAST(IS_NULLABLE AS CHAR)    AS IS_NULLABLE,
                  CHARACTER_MAXIMUM_LENGTH,
                  CAST(COLUMN_DEFAULT AS CHAR) AS COLUMN_DEFAULT,
                  CAST(EXTRA AS CHAR)          AS EXTRA
           FROM information_schema.columns
           WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
           ORDER BY ORDINAL_POSITION"#
    };
    let mut q = sqlx::query(sql);
    if !use_default {
        q = q.bind(schema);
    }
    q = q.bind(table);
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.try_get("COLUMN_NAME").unwrap_or_default();
        let data_type: String = r.try_get("DATA_TYPE").unwrap_or_default();
        let nullable: String = r.try_get("IS_NULLABLE").unwrap_or_else(|_| "NO".into());
        // MySQL's CHARACTER_MAXIMUM_LENGTH is u64 in information_schema —
        // fetch as i64 and downcast (varchar(80) fits trivially).
        let max_length: Option<i32> = r
            .try_get::<Option<i64>, _>("CHARACTER_MAXIMUM_LENGTH")
            .ok()
            .flatten()
            .map(|n| i32::try_from(n).unwrap_or(i32::MAX));
        let default: Option<String> = r.try_get("COLUMN_DEFAULT").ok().flatten();
        let extra: String = r.try_get("EXTRA").unwrap_or_default();
        let is_auto = extra.eq_ignore_ascii_case("auto_increment");
        out.push(ColumnRow {
            name,
            udt_name: data_type,
            nullable: nullable.eq_ignore_ascii_case("YES"),
            max_length,
            default,
            is_auto,
        });
    }
    Ok(out)
}

#[cfg(feature = "mysql")]
async fn list_pk_columns_my(
    pool: &sqlx::MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    // See `list_tables_my` for why every string column is `CAST AS CHAR`.
    let use_default = schema == "public" || schema.is_empty();
    let sql = if use_default {
        r#"SELECT CAST(kcu.COLUMN_NAME AS CHAR) AS COLUMN_NAME
           FROM information_schema.TABLE_CONSTRAINTS tc
           JOIN information_schema.KEY_COLUMN_USAGE kcu
             ON tc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME
            AND tc.TABLE_SCHEMA = kcu.TABLE_SCHEMA
            AND tc.TABLE_NAME = kcu.TABLE_NAME
           WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY'
             AND tc.TABLE_SCHEMA = DATABASE() AND tc.TABLE_NAME = ?
           ORDER BY kcu.ORDINAL_POSITION"#
    } else {
        r#"SELECT CAST(kcu.COLUMN_NAME AS CHAR) AS COLUMN_NAME
           FROM information_schema.TABLE_CONSTRAINTS tc
           JOIN information_schema.KEY_COLUMN_USAGE kcu
             ON tc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME
            AND tc.TABLE_SCHEMA = kcu.TABLE_SCHEMA
            AND tc.TABLE_NAME = kcu.TABLE_NAME
           WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY'
             AND tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ?
           ORDER BY kcu.ORDINAL_POSITION"#
    };
    let mut q = sqlx::query(sql);
    if !use_default {
        q = q.bind(schema);
    }
    q = q.bind(table);
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("COLUMN_NAME").unwrap_or_default())
        .collect())
}

#[cfg(feature = "mysql")]
async fn list_fks_my(
    pool: &sqlx::MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<(String, String)>, MigrateError> {
    use sqlx::Row as _;
    // See `list_tables_my` for why every string column is `CAST AS CHAR`.
    let use_default = schema == "public" || schema.is_empty();
    let sql = if use_default {
        r#"SELECT CAST(COLUMN_NAME AS CHAR)            AS COLUMN_NAME,
                  CAST(REFERENCED_TABLE_NAME AS CHAR)  AS REFERENCED_TABLE_NAME
           FROM information_schema.KEY_COLUMN_USAGE
           WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL
           ORDER BY ORDINAL_POSITION"#
    } else {
        r#"SELECT CAST(COLUMN_NAME AS CHAR)            AS COLUMN_NAME,
                  CAST(REFERENCED_TABLE_NAME AS CHAR)  AS REFERENCED_TABLE_NAME
           FROM information_schema.KEY_COLUMN_USAGE
           WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL
           ORDER BY ORDINAL_POSITION"#
    };
    let mut q = sqlx::query(sql);
    if !use_default {
        q = q.bind(schema);
    }
    q = q.bind(table);
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.try_get::<String, _>("COLUMN_NAME").unwrap_or_default(),
                r.try_get::<String, _>("REFERENCED_TABLE_NAME")
                    .unwrap_or_default(),
            )
        })
        .collect())
}

#[cfg(feature = "mysql")]
async fn list_views_my(
    pool: &sqlx::MySqlPool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    // See `list_tables_my` for why every string column is `CAST AS CHAR`.
    let use_default = schema == "public" || schema.is_empty();
    let sql = match (only, use_default) {
        (Some(_), true) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.views
               WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?
               ORDER BY TABLE_NAME"#
        }
        (Some(_), false) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.views
               WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
               ORDER BY TABLE_NAME"#
        }
        (None, true) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.views
               WHERE TABLE_SCHEMA = DATABASE()
               ORDER BY TABLE_NAME"#
        }
        (None, false) => {
            r#"SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.views
               WHERE TABLE_SCHEMA = ?
               ORDER BY TABLE_NAME"#
        }
    };
    let mut q = sqlx::query(sql);
    if !use_default {
        q = q.bind(schema);
    }
    if let Some(v) = only {
        q = q.bind(v);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("TABLE_NAME").unwrap_or_default())
        .collect())
}

#[cfg(feature = "mysql")]
async fn view_definition_my(
    pool: &sqlx::MySqlPool,
    schema: &str,
    view: &str,
) -> Result<Option<String>, MigrateError> {
    let use_default = schema == "public" || schema.is_empty();
    let sql = if use_default {
        r#"SELECT CAST(VIEW_DEFINITION AS CHAR) AS VIEW_DEFINITION
           FROM information_schema.views
           WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?"#
    } else {
        r#"SELECT CAST(VIEW_DEFINITION AS CHAR) AS VIEW_DEFINITION
           FROM information_schema.views
           WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?"#
    };
    let mut q = sqlx::query_scalar::<_, Option<String>>(sql);
    if !use_default {
        q = q.bind(schema);
    }
    q = q.bind(view);
    let row = q.fetch_optional(pool).await.map_err(MigrateError::Driver)?;
    Ok(row.flatten())
}

// -------------------------------------------------------------- SQLite

#[cfg(feature = "sqlite")]
async fn list_tables_sqlite(
    pool: &sqlx::SqlitePool,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    let sql = if only.is_some() {
        // `sqlite_sequence` and other sqlite-internal tables aren't
        // user tables — filter the `sqlite_%` prefix.
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name = ? ORDER BY name"
    } else {
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    };
    let mut q = sqlx::query(sql);
    if let Some(t) = only {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
        .collect())
}

#[cfg(feature = "sqlite")]
async fn list_columns_sqlite(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Result<Vec<ColumnRow>, MigrateError> {
    use sqlx::Row as _;
    // PRAGMA can't be parameterized — table names go in literally.
    // sqlite_master is the source of truth for declared type +
    // AUTOINCREMENT presence; PRAGMA table_info gives us nullable +
    // default + pk flag.
    let pragma = format!("PRAGMA table_info({})", quote_sqlite_ident(table));
    let rows = sqlx::query(&pragma)
        .fetch_all(pool)
        .await
        .map_err(MigrateError::Driver)?;
    // Pull the table's CREATE TABLE source to detect AUTOINCREMENT —
    // PRAGMA doesn't expose this flag.
    let create_sql: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(table)
    .fetch_optional(pool)
    .await
    .map_err(MigrateError::Driver)?;
    let has_autoincrement = create_sql
        .as_deref()
        .is_some_and(|s| s.to_ascii_uppercase().contains("AUTOINCREMENT"));
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.try_get("name").unwrap_or_default();
        let declared_ty: String = r.try_get("type").unwrap_or_default();
        let notnull: i64 = r.try_get("notnull").unwrap_or(0);
        let default: Option<String> = r.try_get("dflt_value").ok().flatten();
        let pk_flag: i64 = r.try_get("pk").unwrap_or(0);
        // INTEGER PRIMARY KEY columns are aliases for ROWID and are
        // implicitly auto-increment. `AUTOINCREMENT` keyword is the
        // stricter (gap-free) variant — detect either.
        let upper = declared_ty.to_ascii_uppercase();
        let is_int_pk = pk_flag > 0 && (upper == "INTEGER" || upper.starts_with("INT"));
        let is_auto = (is_int_pk && pk_flag == 1) || (has_autoincrement && pk_flag == 1);
        // Strip `VARCHAR(80)` parens for max_length detection.
        let (base_ty, max_length) = parse_sqlite_type(&declared_ty);
        out.push(ColumnRow {
            name,
            udt_name: base_ty.to_owned(),
            nullable: notnull == 0 && pk_flag == 0,
            max_length,
            default,
            is_auto,
        });
    }
    Ok(out)
}

#[cfg(feature = "sqlite")]
async fn list_pk_columns_sqlite(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    let pragma = format!("PRAGMA table_info({})", quote_sqlite_ident(table));
    let rows = sqlx::query(&pragma)
        .fetch_all(pool)
        .await
        .map_err(MigrateError::Driver)?;
    let mut pk_pairs: Vec<(i64, String)> = rows
        .into_iter()
        .filter_map(|r| {
            let pk: i64 = r.try_get("pk").ok()?;
            if pk > 0 {
                let name: String = r.try_get("name").ok()?;
                Some((pk, name))
            } else {
                None
            }
        })
        .collect();
    // `pk` is the ordinal in the primary key — sort to get the
    // canonical composite order.
    pk_pairs.sort_by_key(|(ord, _)| *ord);
    Ok(pk_pairs.into_iter().map(|(_, n)| n).collect())
}

#[cfg(feature = "sqlite")]
async fn list_fks_sqlite(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Result<Vec<(String, String)>, MigrateError> {
    use sqlx::Row as _;
    let pragma = format!("PRAGMA foreign_key_list({})", quote_sqlite_ident(table));
    let rows = sqlx::query(&pragma)
        .fetch_all(pool)
        .await
        .map_err(MigrateError::Driver)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let local: String = r.try_get("from").unwrap_or_default();
        let target: String = r.try_get("table").unwrap_or_default();
        out.push((local, target));
    }
    Ok(out)
}

#[cfg(feature = "sqlite")]
async fn list_views_sqlite(
    pool: &sqlx::SqlitePool,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    use sqlx::Row as _;
    let sql = if only.is_some() {
        "SELECT name FROM sqlite_master WHERE type = 'view' AND name NOT LIKE 'sqlite_%' AND name = ? ORDER BY name"
    } else {
        "SELECT name FROM sqlite_master WHERE type = 'view' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    };
    let mut q = sqlx::query(sql);
    if let Some(v) = only {
        q = q.bind(v);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
        .collect())
}

#[cfg(feature = "sqlite")]
async fn view_definition_sqlite(
    pool: &sqlx::SqlitePool,
    view: &str,
) -> Result<Option<String>, MigrateError> {
    let def: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'view' AND name = ?")
            .bind(view)
            .fetch_optional(pool)
            .await
            .map_err(MigrateError::Driver)?
            .flatten();
    Ok(def)
}

#[cfg(feature = "sqlite")]
fn quote_sqlite_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Parse a SQLite declared type like `VARCHAR(80)` → `("varchar", Some(80))`,
/// `INTEGER` → `("INTEGER", None)`. Whitespace-tolerant.
#[cfg(feature = "sqlite")]
fn parse_sqlite_type(s: &str) -> (&str, Option<i32>) {
    let s = s.trim();
    if let Some(open) = s.find('(') {
        if let Some(close) = s[open..].find(')') {
            let inner = s[open + 1..open + close].trim();
            // Take the first numeric token (handles `DECIMAL(10,2)` too).
            let first = inner.split(',').next().unwrap_or(inner).trim();
            if let Ok(n) = first.parse::<i32>() {
                return (s[..open].trim(), Some(n));
            }
        }
    }
    (s, None)
}

// ====================================================================
// Type mapping per backend
// ====================================================================

/// Map a Postgres `udt_name` to a `(rust_type, notes)` tuple.
pub(super) fn pg_type_to_rust(udt_name: &str) -> (&'static str, Option<&'static str>) {
    match udt_name {
        "bool" => ("bool", None),
        "int2" => ("i16", None),
        "int4" => ("i32", None),
        "int8" => ("i64", None),
        "float4" => ("f32", None),
        "float8" => ("f64", None),
        "numeric" => (
            "rust_decimal::Decimal",
            Some("// TODO: numeric → Decimal requires the `rust_decimal` dep"),
        ),
        "varchar" | "bpchar" | "text" | "citext" => ("String", None),
        "uuid" => ("uuid::Uuid", None),
        "jsonb" | "json" => ("serde_json::Value", None),
        "timestamptz" => ("chrono::DateTime<chrono::Utc>", None),
        "timestamp" => (
            "chrono::NaiveDateTime",
            Some("// TODO: `timestamp` (no tz) — prefer `timestamptz` if the column is a real point-in-time"),
        ),
        "date" => ("chrono::NaiveDate", None),
        "time" => ("chrono::NaiveTime", None),
        "bytea" => (
            "Vec<u8>",
            Some("// TODO: `bytea` is not a first-class rustango FieldType yet"),
        ),
        _ => (
            "String",
            Some(
                "// TODO: unknown PG type — falling back to String. \
                 Map to a real Rust type before persisting.",
            ),
        ),
    }
}

/// Map a MySQL `data_type` to a `(rust_type, notes)` tuple.
pub(super) fn mysql_type_to_rust(data_type: &str) -> (&'static str, Option<&'static str>) {
    let t = data_type.to_ascii_lowercase();
    match t.as_str() {
        "tinyint" | "bool" | "boolean" => ("bool", None),
        "smallint" => ("i16", None),
        "mediumint" | "int" | "integer" => ("i32", None),
        "bigint" => ("i64", None),
        "float" => ("f32", None),
        "double" | "real" => ("f64", None),
        "decimal" | "numeric" => (
            "rust_decimal::Decimal",
            Some("// TODO: decimal → Decimal requires the `rust_decimal` dep"),
        ),
        "varchar" | "char" | "text" | "longtext" | "mediumtext" | "tinytext" => ("String", None),
        "json" => ("serde_json::Value", None),
        "datetime" | "timestamp" => ("chrono::DateTime<chrono::Utc>", None),
        "date" => ("chrono::NaiveDate", None),
        "time" => ("chrono::NaiveTime", None),
        "binary" | "varbinary" | "blob" | "longblob" | "mediumblob" | "tinyblob" => (
            "Vec<u8>",
            Some("// TODO: binary types are not first-class rustango FieldType yet"),
        ),
        _ => (
            "String",
            Some(
                "// TODO: unknown MySQL type — falling back to String. \
                 Map to a real Rust type before persisting.",
            ),
        ),
    }
}

/// Map a SQLite declared type (case-insensitive, parens stripped) to
/// a `(rust_type, notes)` tuple. SQLite's affinity rules mean column
/// declared types are loose — common conventions are honored.
pub(super) fn sqlite_type_to_rust(declared: &str) -> (&'static str, Option<&'static str>) {
    let upper = declared.trim().to_ascii_uppercase();
    if upper.contains("INT") {
        return ("i64", None);
    }
    if upper.contains("CHAR")
        || upper.contains("CLOB")
        || upper.contains("TEXT")
        || upper.is_empty()
    {
        return ("String", None);
    }
    if upper.contains("BLOB") {
        return (
            "Vec<u8>",
            Some("// TODO: BLOB columns are not first-class rustango FieldType yet"),
        );
    }
    if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        return ("f64", None);
    }
    if upper.contains("BOOL") {
        return ("bool", None);
    }
    if upper.contains("DATETIME") || upper.contains("TIMESTAMP") {
        return ("chrono::DateTime<chrono::Utc>", None);
    }
    if upper == "DATE" {
        return ("chrono::NaiveDate", None);
    }
    if upper.contains("DECIMAL") || upper.contains("NUMERIC") {
        return (
            "rust_decimal::Decimal",
            Some("// TODO: decimal → Decimal requires the `rust_decimal` dep"),
        );
    }
    (
        "String",
        Some(
            "// TODO: unknown SQLite-declared type — falling back to String. \
             Adjust if the column stores non-text data.",
        ),
    )
}

/// Backend-aware type dispatch used by the emit pass.
fn dialect_type_to_rust(dialect: &str, udt: &str) -> (&'static str, Option<&'static str>) {
    match dialect {
        "postgres" => pg_type_to_rust(udt),
        "mysql" => mysql_type_to_rust(udt),
        "sqlite" => sqlite_type_to_rust(udt),
        _ => pg_type_to_rust(udt),
    }
}

// ====================================================================
// Emit
// ====================================================================

/// PascalCase the table name into a Rust type identifier.
pub(super) fn table_to_struct_name(table: &str) -> String {
    let mut out = String::with_capacity(table.len());
    let mut next_upper = true;
    for c in table.chars() {
        if c == '_' || c == '-' || c == ' ' {
            next_upper = true;
            continue;
        }
        if next_upper {
            out.extend(c.to_uppercase());
            next_upper = false;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push_str("Unnamed");
    }
    out
}

/// Emit one `#[derive(Model)]` block for a table.
#[cfg(test)]
fn write_model<W: Write>(
    w: &mut W,
    table: &str,
    columns: &[ColumnRow],
    pk_columns: &[String],
    fks: &[(String, String)],
) -> std::io::Result<()> {
    write_model_with_dialect(w, table, columns, pk_columns, fks, "postgres")
}

/// Emit one `#[derive(Model)]` block for a SQL view. Issue #293 /
/// T2.10. The struct carries `#[rustango(view)]` so the migration
/// runner skips it (the view is operator-owned). `view_definition` —
/// when present — lands as a fenced doc comment above the struct so
/// reviewers can read the SQL without hunting through `pg_views`.
///
/// Unlike `write_model_with_dialect`, view models do NOT emit a PK
/// attribute or FK attributes: the view's underlying expression is
/// already constrained by the source tables. Users editing the
/// emitted source can add `#[rustango(primary_key)]` to the column
/// they want to use as the ORM-side identity if the view exposes
/// one (Django's `Meta.managed = False` convention).
fn write_view_model_with_dialect<W: Write>(
    w: &mut W,
    view: &str,
    columns: &[ColumnRow],
    definition: Option<&str>,
    dialect: &str,
) -> std::io::Result<()> {
    let struct_name = table_to_struct_name(view);
    writeln!(w, "/// Auto-emitted from view `{view}` by `inspectdb`.")?;
    writeln!(w, "///")?;
    writeln!(
        w,
        "/// This struct is backed by a SQL **view** — `#[rustango(view)]` tells the"
    )?;
    writeln!(
        w,
        "/// migration runner to skip `CREATE TABLE` / `DROP TABLE` against it."
    )?;
    if let Some(def) = definition {
        let trimmed = def.trim();
        if !trimmed.is_empty() {
            writeln!(w, "///")?;
            writeln!(w, "/// ```sql")?;
            for line in trimmed.lines() {
                writeln!(w, "/// {}", escape_for_comment(line))?;
            }
            writeln!(w, "/// ```")?;
        }
    }
    writeln!(w, "#[derive(Model, Debug, Clone)]")?;
    writeln!(w, r#"#[rustango(table = "{view}", view)]"#)?;
    writeln!(w, "pub struct {struct_name} {{")?;
    for col in columns {
        emit_field(w, col, None, &[], dialect)?;
    }
    writeln!(w, "}}")?;
    Ok(())
}

fn write_model_with_dialect<W: Write>(
    w: &mut W,
    table: &str,
    columns: &[ColumnRow],
    pk_columns: &[String],
    fks: &[(String, String)],
    dialect: &str,
) -> std::io::Result<()> {
    let struct_name = table_to_struct_name(table);
    writeln!(w, "/// Auto-emitted from `{table}` by `inspectdb`.")?;
    if pk_columns.len() > 1 {
        writeln!(
            w,
            "/// NOTE: composite primary key — `inspectdb` picked `{}`. Adjust as needed.",
            pk_columns.first().map(String::as_str).unwrap_or("(none)")
        )?;
    }
    writeln!(w, "#[derive(Model, Debug, Clone)]")?;
    writeln!(w, r#"#[rustango(table = "{table}")]"#)?;
    writeln!(w, "pub struct {struct_name} {{")?;
    let pk_first = pk_columns.first().cloned();
    for col in columns {
        emit_field(w, col, pk_first.as_deref(), fks, dialect)?;
    }
    writeln!(w, "}}")?;
    Ok(())
}

fn emit_field<W: Write>(
    w: &mut W,
    col: &ColumnRow,
    pk_column: Option<&str>,
    fks: &[(String, String)],
    dialect: &str,
) -> std::io::Result<()> {
    let (rust_ty, todo_note) = dialect_type_to_rust(dialect, &col.udt_name);
    let is_pk = pk_column == Some(col.name.as_str());
    let fk_target = fks
        .iter()
        .find(|(local, _)| local == &col.name)
        .map(|(_, target)| target.as_str());

    let mut attrs: Vec<String> = Vec::new();
    if is_pk {
        attrs.push("primary_key".into());
    }
    if let Some(target) = fk_target {
        attrs.push(format!(r#"fk = "{target}""#));
    }
    if let Some(max) = col.max_length {
        let lower = col.udt_name.to_ascii_lowercase();
        if lower == "varchar" || lower == "bpchar" || lower == "char" {
            attrs.push(format!("max_length = {max}"));
        }
    }
    if !col.is_auto {
        if let Some(default) = &col.default {
            // Strip the typecast suffix Postgres adds (`'pending'::varchar`).
            let cleaned = default.split("::").next().unwrap_or(default).trim();
            if !cleaned.is_empty() {
                attrs.push(format!(r#"default = "{cleaned}""#));
            }
        }
    }

    let final_ty = if col.is_auto && is_pk {
        format!("Auto<{rust_ty}>")
    } else if col.nullable {
        format!("Option<{rust_ty}>")
    } else {
        rust_ty.to_owned()
    };

    if let Some(note) = todo_note {
        writeln!(w, "    {note}")?;
    }
    if !attrs.is_empty() {
        writeln!(w, "    #[rustango({})]", attrs.join(", "))?;
    }
    writeln!(w, "    pub {}: {final_ty},", sanitize_field_name(&col.name))?;
    Ok(())
}

fn sanitize_field_name(name: &str) -> String {
    const RUST_KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do",
        "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
    ];
    if RUST_KEYWORDS.iter().any(|k| *k == name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

fn escape_for_comment(s: &str) -> String {
    s.replace('\n', " ").replace("*/", "* /")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_defaults_to_public_schema() {
        let a = parse_inspectdb_args(&[]).unwrap();
        assert_eq!(a.schema, "public");
        assert!(a.table.is_none());
    }

    #[test]
    fn parse_args_picks_up_schema_and_table() {
        let a = parse_inspectdb_args(&[
            "--schema".into(),
            "reporting".into(),
            "--table".into(),
            "events".into(),
        ])
        .unwrap();
        assert_eq!(a.schema, "reporting");
        assert_eq!(a.table.as_deref(), Some("events"));
    }

    #[test]
    fn parse_args_rejects_missing_value() {
        assert!(parse_inspectdb_args(&["--schema".into()]).is_err());
        assert!(parse_inspectdb_args(&["--table".into()]).is_err());
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_inspectdb_args(&["--bogus".into()]).unwrap_err();
        assert!(format!("{err}").contains("--bogus"));
    }

    #[test]
    fn pg_type_to_rust_covers_common_types() {
        assert_eq!(pg_type_to_rust("int8").0, "i64");
        assert_eq!(pg_type_to_rust("int4").0, "i32");
        assert_eq!(pg_type_to_rust("varchar").0, "String");
        assert_eq!(pg_type_to_rust("uuid").0, "uuid::Uuid");
        assert_eq!(
            pg_type_to_rust("timestamptz").0,
            "chrono::DateTime<chrono::Utc>"
        );
        assert_eq!(pg_type_to_rust("jsonb").0, "serde_json::Value");
        assert_eq!(pg_type_to_rust("bool").0, "bool");
    }

    #[test]
    fn pg_type_to_rust_unknown_falls_back_to_string_with_note() {
        let (ty, note) = pg_type_to_rust("hstore");
        assert_eq!(ty, "String");
        assert!(note.is_some_and(|n| n.contains("TODO")));
    }

    #[test]
    fn mysql_type_to_rust_covers_common_types() {
        assert_eq!(mysql_type_to_rust("bigint").0, "i64");
        assert_eq!(mysql_type_to_rust("INT").0, "i32");
        assert_eq!(mysql_type_to_rust("VARCHAR").0, "String");
        assert_eq!(mysql_type_to_rust("json").0, "serde_json::Value");
        assert_eq!(
            mysql_type_to_rust("DATETIME").0,
            "chrono::DateTime<chrono::Utc>"
        );
        assert_eq!(mysql_type_to_rust("BOOLEAN").0, "bool");
        assert_eq!(mysql_type_to_rust("tinyint").0, "bool");
    }

    #[test]
    fn sqlite_type_to_rust_handles_affinity_rules() {
        assert_eq!(sqlite_type_to_rust("INTEGER").0, "i64");
        assert_eq!(sqlite_type_to_rust("BIGINT").0, "i64");
        assert_eq!(sqlite_type_to_rust("TEXT").0, "String");
        assert_eq!(sqlite_type_to_rust("VARCHAR").0, "String");
        assert_eq!(sqlite_type_to_rust("REAL").0, "f64");
        assert_eq!(sqlite_type_to_rust("BLOB").0, "Vec<u8>");
        assert_eq!(sqlite_type_to_rust("BOOLEAN").0, "bool");
        assert_eq!(
            sqlite_type_to_rust("DATETIME").0,
            "chrono::DateTime<chrono::Utc>"
        );
        assert_eq!(sqlite_type_to_rust("").0, "String");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn parse_sqlite_type_strips_parens_for_max_length() {
        assert_eq!(parse_sqlite_type("VARCHAR(80)"), ("VARCHAR", Some(80)));
        assert_eq!(parse_sqlite_type("INTEGER"), ("INTEGER", None));
        assert_eq!(parse_sqlite_type("DECIMAL(10,2)"), ("DECIMAL", Some(10)));
        assert_eq!(parse_sqlite_type(" CHAR (32) "), ("CHAR", Some(32)));
    }

    #[test]
    fn table_to_struct_name_pascalcases() {
        assert_eq!(table_to_struct_name("user"), "User");
        assert_eq!(table_to_struct_name("user_profile"), "UserProfile");
        assert_eq!(table_to_struct_name("audit_log_entry"), "AuditLogEntry");
        assert_eq!(table_to_struct_name(""), "Unnamed");
    }

    #[test]
    fn sanitize_field_name_wraps_keywords() {
        assert_eq!(sanitize_field_name("type"), "r#type");
        assert_eq!(sanitize_field_name("match"), "r#match");
        assert_eq!(sanitize_field_name("normal"), "normal");
    }

    #[test]
    fn emit_field_chooses_correct_wrapper_per_state() {
        let mut buf: Vec<u8> = Vec::new();
        let cols = [
            ColumnRow {
                name: "id".into(),
                udt_name: "int8".into(),
                nullable: false,
                max_length: None,
                default: Some("nextval('users_id_seq')".into()),
                is_auto: true,
            },
            ColumnRow {
                name: "name".into(),
                udt_name: "varchar".into(),
                nullable: false,
                max_length: Some(80),
                default: None,
                is_auto: false,
            },
            ColumnRow {
                name: "bio".into(),
                udt_name: "text".into(),
                nullable: true,
                max_length: None,
                default: None,
                is_auto: false,
            },
        ];
        for col in &cols {
            emit_field(&mut buf, col, Some("id"), &[], "postgres").unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("pub id: Auto<i64>"), "got: {out}");
        assert!(out.contains("pub name: String"), "got: {out}");
        assert!(out.contains("pub bio: Option<String>"), "got: {out}");
        assert!(out.contains("max_length = 80"));
        assert!(out.contains("primary_key"));
    }

    #[test]
    fn emit_field_attaches_fk_attribute() {
        let mut buf: Vec<u8> = Vec::new();
        let col = ColumnRow {
            name: "author_id".into(),
            udt_name: "int8".into(),
            nullable: false,
            max_length: None,
            default: None,
            is_auto: false,
        };
        emit_field(
            &mut buf,
            &col,
            Some("id"),
            &[("author_id".into(), "users".into())],
            "postgres",
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(r#"fk = "users""#), "got: {out}");
        assert!(out.contains("pub author_id: i64"), "got: {out}");
    }

    #[test]
    fn write_model_handles_composite_pk_with_warning() {
        let mut buf: Vec<u8> = Vec::new();
        let cols = vec![
            ColumnRow {
                name: "user_id".into(),
                udt_name: "int8".into(),
                nullable: false,
                max_length: None,
                default: None,
                is_auto: false,
            },
            ColumnRow {
                name: "role_id".into(),
                udt_name: "int8".into(),
                nullable: false,
                max_length: None,
                default: None,
                is_auto: false,
            },
        ];
        let pk = vec!["user_id".to_owned(), "role_id".to_owned()];
        write_model(&mut buf, "user_roles", &cols, &pk, &[]).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("composite primary key"),
            "expected composite-PK warning, got: {out}"
        );
        assert!(out.contains("pub struct UserRoles"));
    }

    #[test]
    fn emit_field_strips_typecast_from_default() {
        let mut buf: Vec<u8> = Vec::new();
        let col = ColumnRow {
            name: "status".into(),
            udt_name: "varchar".into(),
            nullable: false,
            max_length: Some(20),
            default: Some("'pending'::character varying".into()),
            is_auto: false,
        };
        emit_field(&mut buf, &col, None, &[], "postgres").unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains(r#"default = "'pending'""#),
            "expected stripped default, got: {out}"
        );
        assert!(
            !out.contains("character varying"),
            "typecast leaked through: {out}"
        );
    }

    #[test]
    fn emit_field_drops_nextval_default_when_auto() {
        let mut buf: Vec<u8> = Vec::new();
        let col = ColumnRow {
            name: "id".into(),
            udt_name: "int8".into(),
            nullable: false,
            max_length: None,
            default: Some("nextval('foo_id_seq')".into()),
            is_auto: true,
        };
        emit_field(&mut buf, &col, Some("id"), &[], "postgres").unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains("default ="),
            "nextval default should not surface, got: {out}"
        );
        assert!(out.contains("Auto<i64>"));
    }

    #[test]
    fn escape_for_comment_strips_dangerous_sequences() {
        assert_eq!(escape_for_comment("foo\nbar"), "foo bar");
        assert_eq!(escape_for_comment("a*/b"), "a* /b");
    }
}
