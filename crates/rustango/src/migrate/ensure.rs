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
#[cfg(feature = "mysql")]
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
        if let Some(my) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
            return MYSQL_DUPLICATES.contains(&my.number());
        }
        if let Some(code) = db.code() {
            if matches!(code.as_ref(), PG_DUPLICATE_TABLE | PG_DUPLICATE_OBJECT) {
                return true;
            }
        }
    }
    let msg = format!("{e}").to_lowercase();
    msg.contains("already exists") || msg.contains("duplicate")
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
pub async fn apply_idempotent(
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
}
