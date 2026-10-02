//! Apply rendered DDL idempotently, for the `ensure_*_table` helpers.
//!
//! Five subsystems create their own table on first use — audit, TOTP,
//! passkeys, API keys, permissions. Each swallowed the failure by
//! matching `"already exists"` in the error text, which needed the
//! server to speak English (#1642).
//!
//! **The logged-ERROR symptom is PostgreSQL-only.** A failing
//! `CREATE TABLE` writes two lines to PG's server log and none to
//! MySQL's, so the `IF NOT EXISTS` rewrite runs on PG alone: on MySQL
//! it silences nothing and costs 3.4x, because the no-op now succeeds
//! and is binlogged.

use crate::sql::Pool;

/// MySQL `ER_TABLE_EXISTS_ERROR`, `ER_DUP_KEYNAME`, `ER_FK_DUP_NAME`.
/// Error *numbers*, not `SQLSTATE`s — see [`is_already_exists`].
///
/// Reachable from the tests on every feature set, which is the point:
/// the trap it guards does not need the `mysql` feature to explain.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
const MYSQL_DUPLICATES: &[u16] = &[1050, 1061, 1826];

/// MySQL `ER_DUP_FIELDNAME`: "already exists" only for an `ADD COLUMN`.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
const MYSQL_DUP_FIELDNAME: u16 = 1060;

/// `true` when `number` is MySQL's way of saying the object `stmt`
/// creates is there.
///
/// Split out so a test can pin the trap without a driver error: these
/// are error numbers, and comparing one against `DatabaseError::code()`
/// — which is the `SQLSTATE` — silently never matches.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
fn is_mysql_duplicate(number: u16, stmt: &str) -> bool {
    MYSQL_DUPLICATES.contains(&number) || (number == MYSQL_DUP_FIELDNAME && is_add_column(stmt))
}

fn is_add_column(stmt: &str) -> bool {
    stmt.starts_with("ALTER TABLE ") && stmt.contains(" ADD COLUMN ")
}

/// `true` when the error says the object is already there.
///
/// Dispatched on the dialect rather than tried in sequence, so a
/// backend that reports a code gets decided by that code. Falling
/// through to the message after a code said "no" is what let MySQL
/// `ER_DUP_ENTRY` — a genuinely failed unique index — read as success.
///
/// * **Postgres** delegates to [`crate::sql::is_pg_dup_object_error`],
///   which also covers the `23505` concurrent-create race (#1458).
/// * **MySQL** matches `number()`, because its `SQLSTATE`s are far too
///   coarse: `42000` is also a syntax error and TEXT-in-index (#1646).
///   Deliberately *not* [`crate::sql::is_mysql_dup_index_error`], which
///   matches that catch-all.
/// * **SQLite** reports nothing usable either way, so it keeps the text
///   match. Its messages are not localised.
#[cfg_attr(not(feature = "mysql"), allow(unused_variables))]
pub(crate) fn is_already_exists(e: &crate::sql::ExecError, dialect: &str, stmt: &str) -> bool {
    let crate::sql::ExecError::Driver(err) = e else {
        return false;
    };
    match dialect {
        "postgres" => crate::sql::is_pg_dup_object_error(err),
        #[cfg(feature = "mysql")]
        "mysql" => err
            .as_database_error()
            .and_then(|db| db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
            .is_some_and(|my| is_mysql_duplicate(my.number(), stmt)),
        _ => {
            let msg = format!("{e}").to_lowercase();
            msg.contains("already exists") || msg.contains("duplicate")
        }
    }
}

/// `CREATE TABLE x` -> `CREATE TABLE IF NOT EXISTS x`, on Postgres only.
///
/// The point is PG's server log, not capability — all three backends
/// accept the syntax. On MySQL the rewrite turns a cheap client-side
/// error into a successful statement that gets binlogged and fsynced,
/// 165 -> 568 us, to silence a log line MySQL never wrote.
///
/// `ALTER TABLE … ADD COLUMN` gets `IF NOT EXISTS` the same way.
/// `ADD CONSTRAINT` is left alone everywhere: no backend has
/// `IF NOT EXISTS` for it, so those still rely on [`is_already_exists`].
fn idempotent<'a>(stmt: &'a str, dialect: &str) -> std::borrow::Cow<'a, str> {
    if dialect != "postgres" {
        return std::borrow::Cow::Borrowed(stmt);
    }
    if let Some(rest) = stmt.strip_prefix("CREATE TABLE ") {
        if !rest.starts_with("IF NOT EXISTS") {
            return std::borrow::Cow::Owned(format!("CREATE TABLE IF NOT EXISTS {rest}"));
        }
    }
    if is_add_column(stmt) && !stmt.contains(" ADD COLUMN IF NOT EXISTS ") {
        if let Some((head, tail)) = stmt.split_once(" ADD COLUMN ") {
            return std::borrow::Cow::Owned(format!("{head} ADD COLUMN IF NOT EXISTS {tail}"));
        }
    }
    std::borrow::Cow::Borrowed(stmt)
}

/// Create every table in `snapshot`, tolerating objects that already
/// exist. The one entry point the `ensure_*` helpers share, so the
/// policy cannot drift between them.
///
/// FK targets are pinned to the schema the tables are created in, so a
/// missing target fails instead of binding to another schema (#1645).
///
/// # Errors
/// Any driver failure that is not "already exists".
pub(crate) async fn apply_idempotent(
    pool: &Pool,
    snapshot: &super::SchemaSnapshot,
) -> Result<(), sqlx::Error> {
    let changes = super::detect_changes(&super::SchemaSnapshot::default(), snapshot);
    apply_changes_idempotent(pool, snapshot, &changes).await
}

/// Add each `(table, column)` of `snapshot` that a table created by an
/// older release lacks. Never creates the table.
///
/// # Errors
/// Any driver failure that is not "already exists", including a
/// missing table.
#[cfg(all(feature = "admin", feature = "totp"))]
pub(crate) async fn add_columns_idempotent(
    pool: &Pool,
    snapshot: &super::SchemaSnapshot,
    columns: &[(&str, &str)],
) -> Result<(), sqlx::Error> {
    let changes: Vec<_> = columns
        .iter()
        .map(|(table, column)| super::SchemaChange::AddColumn {
            table: (*table).to_owned(),
            column: (*column).to_owned(),
        })
        .collect();
    apply_changes_idempotent(pool, snapshot, &changes).await
}

/// Render `changes` against `snapshot` and run them, tolerating objects
/// that already exist.
///
/// # Errors
/// A render failure, or any driver failure that is not "already exists".
pub(crate) async fn apply_changes_idempotent(
    pool: &Pool,
    snapshot: &super::SchemaSnapshot,
    changes: &[super::SchemaChange],
) -> Result<(), sqlx::Error> {
    let schema = creation_schema(pool).await?;
    let batch = super::diff::render_changes_split_in_schema(
        changes,
        snapshot,
        pool.dialect(),
        schema.as_deref(),
    )
    .map_err(sqlx::Error::Protocol)?;
    apply_batch(pool, &batch).await
}

/// Column names `table` has in the schema new tables land in.
///
/// # Errors
/// Driver failures from the catalog read.
pub(crate) async fn live_columns(
    pool: &Pool,
    table: &str,
) -> Result<std::collections::HashSet<String>, super::MigrateError> {
    // MySQL reads `""` as `DATABASE()`; SQLite ignores the schema.
    let schema = creation_schema(pool).await?.unwrap_or_default();
    Ok(super::inspectdb::list_columns(pool, &schema, table)
        .await?
        .into_iter()
        .map(|c| c.name)
        .collect())
}

/// Where Postgres creates unqualified tables; `None` on backends
/// without schemas.
pub(crate) async fn creation_schema(pool: &Pool) -> Result<Option<String>, sqlx::Error> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => sqlx::query_scalar::<_, Option<String>>("SELECT current_schema()")
            .fetch_one(pg)
            .await?
            .map(Some)
            .ok_or_else(|| sqlx::Error::Protocol("search_path names no existing schema".into())),
        #[allow(unreachable_patterns)]
        _ => Ok(None),
    }
}

/// Apply each group of `changes` on its own, FKs last, so one object that
/// fails does not block the others. Returns what could not be applied.
///
/// # Errors
/// Only the catalog read for the creation schema.
pub(crate) async fn converge_groups(
    pool: &Pool,
    snapshot: &super::SchemaSnapshot,
    groups: &[Vec<super::SchemaChange>],
) -> Result<Vec<String>, sqlx::Error> {
    use super::SchemaChange as SC;
    let schema = creation_schema(pool).await?;
    let render = |changes: &[SC], snap: &super::SchemaSnapshot| {
        super::diff::render_changes_split_in_schema(
            changes,
            snap,
            pool.dialect(),
            schema.as_deref(),
        )
    };
    let mut failed = Vec::new();
    let mut fks = Vec::new();
    for group in groups {
        let label = match group.first() {
            Some(SC::AddColumn { table, column }) => format!("column `{table}.{column}`"),
            Some(SC::CreateTable(t) | SC::CreateM2MTable { through: t, .. }) => {
                format!("table `{t}`")
            }
            Some(SC::CreateIndex { name, .. }) => format!("index `{name}`"),
            Some(SC::AlterFkOnDelete { table, column, .. }) => {
                format!("foreign key `{table}.{column}`")
            }
            Some(SC::AddCompositeFk { table, name, .. }) => format!("foreign key `{table}.{name}`"),
            _ => continue,
        };
        let mut no_rows = false;
        if let Some(SC::AddColumn { table, column }) = group.first() {
            let f = snapshot.table(table).and_then(|t| t.field(column));
            if f.is_some_and(|f| !f.nullable && f.default.is_none() && !f.primary_key) {
                // An empty table has no rows to fill (#2066).
                no_rows = is_empty(pool, table).await?;
                if !no_rows {
                    failed.push(format!(
                        "{label}: NOT NULL with no default, so rows have no value"
                    ));
                    continue;
                }
            }
        }
        let rendered = if no_rows {
            super::diff::render_changes_split_for_empty(
                group,
                snapshot,
                pool.dialect(),
                schema.as_deref(),
            )
        } else {
            render(group, snapshot)
        };
        let batch = match rendered {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{label}: {e}"));
                continue;
            }
        };
        let Err(e) = run_statements(pool, &batch.immediate).await else {
            fks.push((label, batch.deferred_fks));
            continue;
        };
        // SQLite refuses a non-constant DEFAULT on a table with rows.
        let retry = filled_table_retry(pool.dialect(), snapshot, group, |s| render(group, s))
            .filter(|_| is_non_constant_default(&e));
        match retry {
            Some(Ok(r)) if run_statements(pool, &r.batch.immediate).await.is_ok() => {
                tracing::warn!(target: "rustango::migrate", "{label}: {}", r.warning);
            }
            _ => failed.push(format!("{label}: {e}")),
        }
    }
    for (label, stmts) in fks {
        if let Err(e) = run_statements(pool, &stmts).await {
            failed.push(format!("{label}: {e}"));
        }
    }
    Ok(failed)
}

/// Whether `table` has no rows.
async fn is_empty(pool: &Pool, table: &str) -> Result<bool, sqlx::Error> {
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 AS one FROM {} LIMIT 1) AS probe",
        pool.dialect().quote_ident(table)
    );
    let rows: Vec<(i64,)> = crate::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .map_err(|e| match e {
            crate::sql::ExecError::Driver(d) => d,
            other => sqlx::Error::Protocol(other.to_string()),
        })?;
    Ok(rows.first().is_some_and(|(n,)| *n == 0))
}

/// SQLite's error for a non-constant DEFAULT added to a table with rows.
pub(super) fn is_non_constant_default(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.message().contains("non-constant default"))
}

/// A rendering of a SQLite AddColumn its non-constant DEFAULT refused.
pub(super) struct FilledTableRetry {
    pub(super) batch: super::diff::RenderedBatch,
    pub(super) warning: &'static str,
}

/// The AddColumn of `group` for a SQLite table with rows (#2017): `now()`
/// frozen to the current time, or a UUID column added without its DEFAULT
/// and backfilled per row. The ORM binds both columns on insert.
pub(super) fn filled_table_retry(
    dialect: &dyn crate::sql::Dialect,
    snapshot: &super::SchemaSnapshot,
    group: &[super::SchemaChange],
    render: impl Fn(&super::SchemaSnapshot) -> Result<super::diff::RenderedBatch, String>,
) -> Option<Result<FilledTableRetry, String>> {
    #[cfg(feature = "sqlite")]
    if let ("sqlite", [super::SchemaChange::AddColumn { table, column }]) = (dialect.name(), group)
    {
        let mut t = snapshot.table(table)?.clone();
        let field = t.fields.iter_mut().find(|f| &f.column == column)?;
        let mut backfill = None;
        let warning = if field
            .default
            .as_deref()
            .is_some_and(crate::sql::is_now_expr)
        {
            let now = crate::sql::encode_datetime(chrono::Utc::now());
            field.default = Some(format!("'{now}'"));
            "added with a fixed DEFAULT: SQLite can't add `now()` to a table with rows"
        } else if super::diff::is_uuid_default(field) {
            backfill = Some(super::diff::fill_nulls_sql(table, field, dialect));
            field.default = None;
            field.nullable = true;
            "added nullable with no DEFAULT: SQLite can't add a UUID DEFAULT to a table with rows"
        } else {
            return None;
        };
        let snap = super::SchemaSnapshot {
            tables: vec![t],
            ..Default::default()
        };
        return Some(render(&snap).map(|mut batch| {
            batch.immediate.extend(backfill);
            FilledTableRetry { batch, warning }
        }));
    }
    let _ = (dialect, snapshot, group, render);
    None
}

/// Run every statement in `batch`, tolerating objects that already exist.
async fn apply_batch(pool: &Pool, batch: &super::RenderedBatch) -> Result<(), sqlx::Error> {
    run_statements(pool, &batch.immediate).await?;
    run_statements(pool, &batch.deferred_fks).await
}

/// Run `stmts` in order, tolerating objects that already exist.
async fn run_statements(pool: &Pool, stmts: &[String]) -> Result<(), sqlx::Error> {
    let dialect = pool.dialect().name();
    for stmt in stmts {
        let stmt = idempotent(stmt, dialect);
        if let Err(e) = crate::sql::raw_execute_pool(pool, &stmt, Vec::new()).await {
            if is_already_exists(&e, dialect, &stmt) {
                continue;
            }
            return Err(match e {
                crate::sql::ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{idempotent, is_mysql_duplicate};

    /// Only the refused non-constant DEFAULT is retried, not any error.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn only_a_non_constant_default_is_retried() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
            "INSERT INTO t VALUES (1)",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let run = |sql| sqlx::query(sql).execute(&pool);
        let refused = run("ALTER TABLE t ADD COLUMN c BLOB DEFAULT (randomblob(16))").await;
        assert!(super::is_non_constant_default(&refused.unwrap_err()));
        let other = run("ALTER TABLE t ADD COLUMN id BLOB DEFAULT (randomblob(16))").await;
        assert!(!super::is_non_constant_default(&other.unwrap_err()));
    }

    #[test]
    fn create_table_gains_if_not_exists_on_postgres() {
        assert_eq!(
            idempotent(r#"CREATE TABLE "t" ("id" BIGSERIAL)"#, "postgres"),
            r#"CREATE TABLE IF NOT EXISTS "t" ("id" BIGSERIAL)"#
        );
    }

    /// MySQL never had the logged ERROR this rewrite exists to silence,
    /// and on MySQL the rewritten statement succeeds and is binlogged —
    /// 3.4x slower for nothing.
    #[test]
    fn other_dialects_are_left_alone() {
        let s = r#"CREATE TABLE "t" ("id" BIGSERIAL)"#;
        assert_eq!(idempotent(s, "mysql"), s);
        assert_eq!(idempotent(s, "sqlite"), s);
    }

    #[test]
    fn already_guarded_create_is_left_alone() {
        let s = r#"CREATE TABLE IF NOT EXISTS "t" ("id" BIGSERIAL)"#;
        assert_eq!(idempotent(s, "postgres"), s, "must not double up the guard");
    }

    #[test]
    fn non_create_table_statements_are_untouched() {
        // No backend has `ADD CONSTRAINT IF NOT EXISTS`, so this one
        // still reaches the server and still relies on the code check.
        let s =
            r#"ALTER TABLE "a" ADD CONSTRAINT "a_b_fkey" FOREIGN KEY ("b") REFERENCES "b" ("id")"#;
        assert_eq!(idempotent(s, "postgres"), s);
        let idx = r#"CREATE UNIQUE INDEX "i" ON "t" ("a")"#;
        assert_eq!(idempotent(idx, "postgres"), idx);
    }

    #[test]
    fn add_column_gains_if_not_exists_on_postgres_only() {
        let s = r#"ALTER TABLE "t" ADD COLUMN "c" BIGINT"#;
        let guarded = r#"ALTER TABLE "t" ADD COLUMN IF NOT EXISTS "c" BIGINT"#;
        assert_eq!(idempotent(s, "postgres"), guarded);
        assert_eq!(idempotent(guarded, "postgres"), guarded);
        assert_eq!(idempotent(s, "mysql"), s);
    }

    /// The tests above feed in hand-written SQL, so they would pass even
    /// if the renderer stopped emitting the shape `idempotent` looks
    /// for. This one runs a real batch through the real renderer, which
    /// is the coupling that actually has to hold.
    ///
    /// `ContentType` rather than a tenancy model on purpose: `migrate`
    /// is ungated, and naming a `#[cfg]`-gated module here broke every
    /// build without that feature.
    #[test]
    fn the_renderer_output_is_actually_rewritten() {
        use crate::core::Model as _;
        let snapshot = crate::migrate::SchemaSnapshot::from_models(&[
            crate::contenttypes::ContentType::SCHEMA,
        ]);
        let changes =
            crate::migrate::detect_changes(&crate::migrate::SchemaSnapshot::default(), &snapshot);
        let batch = crate::migrate::render_changes_split_with_dialect(
            &changes,
            &snapshot,
            &crate::sql::Postgres,
        )
        .expect("render");

        let creates: Vec<_> = batch
            .immediate
            .iter()
            .filter(|s| s.contains("CREATE TABLE"))
            .collect();
        assert!(!creates.is_empty(), "expected at least one CREATE TABLE");
        for stmt in creates {
            assert!(
                idempotent(stmt, "postgres").starts_with("CREATE TABLE IF NOT EXISTS"),
                "renderer emits a shape `idempotent` does not rewrite, so the \
                 ensure paths are back to erroring per call: {stmt}"
            );
        }
    }

    /// `apply_batch` itself, against a real SQLite database.
    ///
    /// Written because the review found three reverts of it that every
    /// other test survived: dropping the `idempotent` rewrite, dropping
    /// `.chain(deferred_fks)` so FK statements never run, and making
    /// `is_already_exists` return `true` so every failure is silent.
    /// Each assertion below kills one of them.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn apply_idempotent_runs_both_lists_and_still_propagates() {
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");

        // `deferred_fks` must run too — this table only exists if it does.
        let batch = super::super::RenderedBatch {
            immediate: vec!["CREATE TABLE a (id INTEGER PRIMARY KEY)".to_owned()],
            deferred_fks: vec!["CREATE TABLE b (id INTEGER PRIMARY KEY)".to_owned()],
            ..Default::default()
        };
        super::apply_batch(&pool, &batch)
            .await
            .expect("first run creates both");
        for t in ["a", "b"] {
            crate::sql::raw_execute_pool(&pool, &format!("SELECT 1 FROM {t}"), Vec::new())
                .await
                .unwrap_or_else(|e| panic!("{t} was never created — deferred_fks skipped? {e}"));
        }

        // Second run is a no-op, not an error.
        super::apply_batch(&pool, &batch)
            .await
            .expect("re-running an ensure must be idempotent");

        // A failure that is NOT "already exists" must still surface.
        let bad = super::super::RenderedBatch {
            immediate: vec!["CREATE TABLE c (id INTEGER PRIMARY KEY".to_owned()],
            ..Default::default()
        };
        assert!(
            super::apply_batch(&pool, &bad).await.is_err(),
            "a syntax error must propagate; swallowing everything would \
             make every ensure silently succeed"
        );
    }

    /// A MySQL error *number* is not a `SQLSTATE`. An earlier draft
    /// compared these against `DatabaseError::code()`, which returns the
    /// `SQLSTATE`, so it could never match — and a text fallback hid it.
    #[test]
    fn mysql_duplicates_are_numbers() {
        let create = "CREATE TABLE t (id INT)";
        assert!(is_mysql_duplicate(1050, create), "ER_TABLE_EXISTS_ERROR");
        assert!(is_mysql_duplicate(1061, create), "ER_DUP_KEYNAME");
        assert!(is_mysql_duplicate(1826, create), "ER_FK_DUP_NAME");
        // ER_DUP_ENTRY: a unique index that genuinely could not be
        // built. It must propagate, not read as "already exists".
        assert!(
            !is_mysql_duplicate(1062, create),
            "ER_DUP_ENTRY must not swallow"
        );
        assert!(
            !is_mysql_duplicate(1064, create),
            "syntax error must not swallow"
        );
        assert!(
            !is_mysql_duplicate(1170, create),
            "TEXT-in-index must not swallow"
        );
    }

    /// `ER_DUP_FIELDNAME` means "already there" only for `ADD COLUMN`; a
    /// CREATE TABLE that names a column twice is a real error.
    #[test]
    fn mysql_dup_fieldname_only_for_add_column() {
        assert!(is_mysql_duplicate(
            1060,
            "ALTER TABLE `t` ADD COLUMN `c` BIGINT"
        ));
        assert!(!is_mysql_duplicate(
            1060,
            "CREATE TABLE `t` (`c` INT, `c` INT)"
        ));
    }
}
