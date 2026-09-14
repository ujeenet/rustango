//! `SqlValue::Null` goes into a column of any type (#1450).
//!
//! It was bound as `None::<String>`, which sends the parameter to
//! PostgreSQL with the **text** OID. Postgres then refuses it anywhere
//! but a text column:
//!
//! ```text
//! column "uploaded_by_id" is of type bigint but expression is of type text
//! ```
//!
//! So writing NULL to any non-text column failed outright. Media upload
//! was broken on Postgres — `uploaded_by_id: None` is the ordinary case
//! for an anonymous or system upload — and 50-odd other sites across
//! `soft_delete`, `audit`, `fixtures`, `forms`, `viewset`, `admin` and
//! `migrate` share the same expression.
//!
//! MySQL and SQLite type parameters loosely enough to accept a text NULL
//! in a bigint column, which is why this was Postgres-only and why it
//! survived a tri-dialect test suite: the two backends that pass tell you
//! nothing about the one that does not. The Postgres arm below is the
//! test that matters; the SQLite arm is there so the property is pinned
//! somewhere that runs without a service.
//!
//! The fix binds an OID-0 NULL — the wire protocol's "unspecified" — so
//! the server infers the type from the column during Parse.

#![cfg(feature = "sqlite")]

use rustango::core::SqlValue;
use rustango::sql::Pool;

/// Every column here is a different type, and all of them take a NULL.
/// A bigint is the case that actually failed; the rest are there because
/// "it works for bigint" is not the claim — "it works regardless of the
/// column's type" is.
const SQLITE_DDL: &str = r#"CREATE TABLE nulls_probe (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    who  BIGINT,
    when_ TEXT,
    blob  TEXT,
    note  TEXT
)"#;

#[tokio::test]
async fn sqlite_accepts_a_null_in_a_bigint_column() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(&pool, SQLITE_DDL, Vec::new())
        .await
        .expect("create");

    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO nulls_probe (who, when_, blob, note) VALUES (?, ?, ?, ?)",
        vec![
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Null,
        ],
    )
    .await
    .expect("a NULL must be writable into every column type");

    // Irrefutable in a sqlite-only build, refutable once another backend
    // feature is on.
    #[allow(irrefutable_let_patterns)]
    let Pool::Sqlite(sq) = &pool
    else {
        unreachable!("connected to sqlite")
    };
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM nulls_probe WHERE who IS NULL AND note IS NULL")
            .fetch_one(sq)
            .await
            .expect("count");
    assert_eq!(n, 1, "the row must hold real NULLs, not coerced values");
}

/// The arm that reproduces #1450. Skipped when `DATABASE_URL` is unset;
/// **fails loudly** when it is set but unreachable, rather than passing
/// silently — the failure mode of #1434 and #1440.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_accepts_a_null_in_a_bigint_column() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set — skipping the PG arm of the #1450 guard");
        return;
    };
    let pg = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));

    for sql in [
        "DROP TABLE IF EXISTS nulls_probe",
        r#"CREATE TABLE nulls_probe (
            id    BIGSERIAL PRIMARY KEY,
            who   BIGINT,
            when_ TIMESTAMPTZ,
            blob  JSONB,
            note  TEXT
        )"#,
    ] {
        sqlx::query(sql).execute(&pg).await.expect("ddl");
    }

    let pool = Pool::Postgres(pg.clone());
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO nulls_probe (who, when_, blob, note) VALUES ($1, $2, $3, $4)",
        vec![
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Null,
        ],
    )
    .await
    .expect(
        "a NULL must be writable into bigint / timestamptz / jsonb, \
         not only text — this is #1450",
    );

    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM nulls_probe \
         WHERE who IS NULL AND when_ IS NULL AND blob IS NULL AND note IS NULL",
    )
    .fetch_one(&pg)
    .await
    .expect("count");
    assert_eq!(n, 1, "the row must hold real NULLs, not coerced values");

    sqlx::query("DROP TABLE nulls_probe")
        .execute(&pg)
        .await
        .ok();
}
