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

/// `true` when `number` is MySQL's way of saying the object is there.
///
/// Split out so a test can pin the trap without a driver error: these
/// are error numbers, and comparing one against `DatabaseError::code()`
/// — which is the `SQLSTATE` — silently never matches.
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
fn is_mysql_duplicate(number: u16) -> bool {
    MYSQL_DUPLICATES.contains(&number)
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
fn is_already_exists(e: &crate::sql::ExecError, dialect: &str) -> bool {
    let crate::sql::ExecError::Driver(err) = e else {
        return false;
    };
    match dialect {
        "postgres" => crate::sql::is_pg_dup_object_error(err),
        #[cfg(feature = "mysql")]
        "mysql" => err
            .as_database_error()
            .and_then(|db| db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
            .is_some_and(|my| is_mysql_duplicate(my.number())),
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
/// `ADD CONSTRAINT` is left alone everywhere: no backend has
/// `IF NOT EXISTS` for it, so those still rely on [`is_already_exists`].
fn idempotent<'a>(stmt: &'a str, dialect: &str) -> std::borrow::Cow<'a, str> {
    if dialect != "postgres" {
        return std::borrow::Cow::Borrowed(stmt);
    }
    match stmt.strip_prefix("CREATE TABLE ") {
        Some(rest) if !rest.starts_with("IF NOT EXISTS") => {
            std::borrow::Cow::Owned(format!("CREATE TABLE IF NOT EXISTS {rest}"))
        }
        _ => std::borrow::Cow::Borrowed(stmt),
    }
}

/// Run every statement in `batch`, tolerating objects that already
/// exist. The one entry point the `ensure_*` helpers share, so the
/// policy cannot drift between them.
///
/// # Errors
/// Any driver failure that is not "already exists".
pub(crate) async fn apply_idempotent(
    pool: &Pool,
    batch: &super::RenderedBatch,
) -> Result<(), sqlx::Error> {
    let dialect = pool.dialect().name();
    for stmt in batch.immediate.iter().chain(batch.deferred_fks.iter()) {
        let stmt = idempotent(stmt, dialect);
        if let Err(e) = crate::sql::raw_execute_pool(pool, &stmt, Vec::new()).await {
            if is_already_exists(&e, dialect) {
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

    /// `apply_idempotent` itself, against a real SQLite database.
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
        super::apply_idempotent(&pool, &batch)
            .await
            .expect("first run creates both");
        for t in ["a", "b"] {
            crate::sql::raw_execute_pool(&pool, &format!("SELECT 1 FROM {t}"), Vec::new())
                .await
                .unwrap_or_else(|e| panic!("{t} was never created — deferred_fks skipped? {e}"));
        }

        // Second run is a no-op, not an error.
        super::apply_idempotent(&pool, &batch)
            .await
            .expect("re-running an ensure must be idempotent");

        // A failure that is NOT "already exists" must still surface.
        let bad = super::super::RenderedBatch {
            immediate: vec!["CREATE TABLE c (id INTEGER PRIMARY KEY".to_owned()],
            ..Default::default()
        };
        assert!(
            super::apply_idempotent(&pool, &bad).await.is_err(),
            "a syntax error must propagate; swallowing everything would \
             make every ensure silently succeed"
        );
    }

    /// A MySQL error *number* is not a `SQLSTATE`. An earlier draft
    /// compared these against `DatabaseError::code()`, which returns the
    /// `SQLSTATE`, so it could never match — and a text fallback hid it.
    #[test]
    fn mysql_duplicates_are_numbers() {
        assert!(is_mysql_duplicate(1050), "ER_TABLE_EXISTS_ERROR");
        assert!(is_mysql_duplicate(1061), "ER_DUP_KEYNAME");
        assert!(is_mysql_duplicate(1826), "ER_FK_DUP_NAME");
        // ER_DUP_ENTRY: a unique index that genuinely could not be
        // built. It must propagate, not read as "already exists".
        assert!(!is_mysql_duplicate(1062), "ER_DUP_ENTRY must not swallow");
        assert!(!is_mysql_duplicate(1064), "syntax error must not swallow");
        assert!(!is_mysql_duplicate(1170), "TEXT-in-index must not swallow");
    }
}
