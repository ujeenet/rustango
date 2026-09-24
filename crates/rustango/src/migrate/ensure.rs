//! Apply rendered DDL idempotently, for the `ensure_*_table` helpers.
//!
//! Five subsystems create their own table on first use — audit, TOTP,
//! passkeys, API keys, permissions. Each rendered DDL through
//! [`super::render_changes_split_with_dialect`] and then swallowed the
//! failure by matching `"already exists"` in the error text. That put an
//! ERROR in the server log on every call after the first, and the match
//! depended on the database speaking English (#1642).

use crate::sql::Pool;

/// Postgres `duplicate_table`.
const PG_DUPLICATE_TABLE: &str = "42P07";
/// Postgres `duplicate_object` — what `ADD CONSTRAINT` raises.
const PG_DUPLICATE_OBJECT: &str = "42710";
/// MySQL `ER_TABLE_EXISTS_ERROR`, `ER_DUP_KEYNAME`, `ER_FK_DUP_NAME`.
/// These are error *numbers*, not `SQLSTATE`s — see below.
const MYSQL_DUPLICATES: &[u16] = &[1050, 1061, 1826];

/// `true` when the error says the object is already there.
///
/// Prefers the backend's code, which does not change with the server's
/// language. Two traps, both already documented in
/// [`crate::sql::connect_diagnosis`]:
///
/// * `DatabaseError::code()` is the `SQLSTATE`. MySQL's are far too
///   coarse to use here, so MySQL is matched on `number()` via
///   downcast.
/// * SQLite reports nothing useful either way, so it keeps the text
///   match. Its messages are not localised.
fn is_already_exists(e: &crate::sql::ExecError) -> bool {
    let crate::sql::ExecError::Driver(err) = e else {
        return false;
    };
    if let Some(db) = err.as_database_error() {
        #[cfg(feature = "mysql")]
        let number = db
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .map(sqlx::mysql::MySqlDatabaseError::number);
        #[cfg(not(feature = "mysql"))]
        let number: Option<u16> = None;
        if is_duplicate_code(db.code().as_deref(), number) {
            return true;
        }
    }
    let msg = format!("{e}").to_lowercase();
    msg.contains("already exists") || msg.contains("duplicate")
}

/// The code decision on its own, over the raw values, so a test can
/// reach it without constructing a driver error.
///
/// `sqlstate` is `DatabaseError::code()`; `mysql_number` is the MySQL
/// error number, which lives somewhere else entirely. Keeping them as
/// separate arguments is the point — comparing one against the other
/// is the bug this function exists to make visible.
fn is_duplicate_code(sqlstate: Option<&str>, mysql_number: Option<u16>) -> bool {
    if let Some(n) = mysql_number {
        return MYSQL_DUPLICATES.contains(&n);
    }
    matches!(sqlstate, Some(PG_DUPLICATE_TABLE | PG_DUPLICATE_OBJECT))
}

/// `CREATE TABLE x` -> `CREATE TABLE IF NOT EXISTS x`, so the common
/// case stops going to the server as an error at all. Same rewrite
/// [`crate::server::app`] does on the boot path.
///
/// `ADD CONSTRAINT` gets no such treatment: Postgres has no
/// `IF NOT EXISTS` for it, so those still rely on [`is_already_exists`].
fn idempotent(stmt: &str) -> std::borrow::Cow<'_, str> {
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
    for stmt in batch.immediate.iter().chain(batch.deferred_fks.iter()) {
        let stmt = idempotent(stmt);
        if let Err(e) = crate::sql::raw_execute_pool(pool, &stmt, Vec::new()).await {
            if is_already_exists(&e) {
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
    use super::idempotent;

    #[test]
    fn create_table_gains_if_not_exists() {
        assert_eq!(
            idempotent(r#"CREATE TABLE "t" ("id" BIGSERIAL)"#),
            r#"CREATE TABLE IF NOT EXISTS "t" ("id" BIGSERIAL)"#
        );
    }

    #[test]
    fn already_guarded_create_is_left_alone() {
        let s = r#"CREATE TABLE IF NOT EXISTS "t" ("id" BIGSERIAL)"#;
        assert_eq!(idempotent(s), s, "must not double up the guard");
    }

    #[test]
    fn other_statements_are_untouched() {
        // Postgres has no `ADD CONSTRAINT IF NOT EXISTS`, so this one
        // still reaches the server and still relies on the code check.
        let s =
            r#"ALTER TABLE "a" ADD CONSTRAINT "a_b_fkey" FOREIGN KEY ("b") REFERENCES "b" ("id")"#;
        assert_eq!(idempotent(s), s);
        let idx = r#"CREATE UNIQUE INDEX "i" ON "t" ("a")"#;
        assert_eq!(idempotent(idx), idx);
    }

    /// The tests above feed in hand-written SQL, so they would pass
    /// even if the renderer stopped emitting the shape `idempotent`
    /// looks for. This one runs a real batch through the real
    /// renderer, which is the coupling that actually has to hold.
    #[test]
    fn the_renderer_output_is_actually_rewritten() {
        use crate::core::Model as _;
        let snapshot = crate::migrate::SchemaSnapshot::from_models(&[
            crate::tenancy::permissions::Permission::SCHEMA,
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
                idempotent(stmt).starts_with("CREATE TABLE IF NOT EXISTS"),
                "renderer emits a shape `idempotent` does not rewrite, so the \
                 ensure paths are back to erroring per call: {stmt}"
            );
        }
    }

    /// The exact mistake this function exists to prevent: a MySQL
    /// error *number* is not a `SQLSTATE`, and comparing one as the
    /// other silently never matches.
    #[test]
    fn a_mysql_number_is_not_read_as_a_sqlstate() {
        use super::is_duplicate_code;
        assert!(is_duplicate_code(None, Some(1050)), "MySQL table exists");
        assert!(is_duplicate_code(None, Some(1061)), "MySQL dup key name");
        assert!(is_duplicate_code(None, Some(1826)), "MySQL dup FK name");
        assert!(
            !is_duplicate_code(Some("1050"), None),
            "an error number arriving as a SQLSTATE must not match — the \
             first draft did exactly this and the text fallback hid it"
        );
        assert!(is_duplicate_code(Some("42P07"), None), "PG duplicate_table");
        assert!(
            is_duplicate_code(Some("42710"), None),
            "PG duplicate_object, what ADD CONSTRAINT raises"
        );
        assert!(!is_duplicate_code(Some("42P01"), None), "undefined_table");
        assert!(!is_duplicate_code(None, None));
    }
}
