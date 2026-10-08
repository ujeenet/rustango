//! Issue #344 — `#[rustango(citext)]` field attribute that
//! routes the migration DDL through `dialect.ci_text_type` instead
//! of the plain `column_type` mapping.
//!
//! Verifies:
//! 1. The flag threads from macro → `FieldSchema::case_insensitive`.
//! 2. PG emits `CITEXT`, SQLite emits `TEXT COLLATE NOCASE`, MySQL
//!    emits `LONGTEXT COLLATE utf8mb4_general_ci`.
//! 3. PG's `ci_text_extension_sql()` returns the right prelude.
//! 4. Other field types (`i64`, `DateTime`) ignore the flag — the
//!    macro accepts it but the DDL falls through to the normal type.

#![cfg(feature = "sqlite")]

use rustango::core::Model;
use rustango::migrate::ddl::create_table_sql_with_dialect;
use rustango::sql::{Dialect as _, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "ci_users")]
#[allow(dead_code)]
pub struct CiUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(citext, max_length = 200)]
    email: String,
    // Plain text — no flag — used as the contrast control.
    bio: String,
}

#[test]
fn citext_attr_threads_into_field_schema() {
    let schema = <CiUser as Model>::SCHEMA;
    let email = schema
        .fields
        .iter()
        .find(|f| f.name == "email")
        .expect("email field");
    let bio = schema
        .fields
        .iter()
        .find(|f| f.name == "bio")
        .expect("bio field");
    assert!(email.case_insensitive, "email should be case_insensitive");
    assert!(!bio.case_insensitive, "bio should NOT be case_insensitive");
}

#[test]
fn postgres_emits_citext_column_type() {
    let sql = create_table_sql_with_dialect(&Postgres, <CiUser as Model>::SCHEMA);
    assert!(
        sql.contains("\"email\" CITEXT"),
        "expected CITEXT on PG, got: {sql}"
    );
    // Plain field stays VARCHAR/TEXT.
    assert!(
        !sql.contains("\"bio\" CITEXT"),
        "bio must not be CITEXT: {sql}"
    );
}

#[test]
fn sqlite_emits_text_collate_nocase() {
    let sql = create_table_sql_with_dialect(&Sqlite, <CiUser as Model>::SCHEMA);
    assert!(
        sql.contains("\"email\" TEXT COLLATE NOCASE"),
        "expected TEXT COLLATE NOCASE on SQLite, got: {sql}"
    );
    assert!(
        !sql.contains("\"bio\" TEXT COLLATE NOCASE"),
        "bio must not be COLLATE NOCASE: {sql}"
    );
}

#[test]
fn mysql_emits_varchar_collate_utf8mb4_general_ci() {
    let sql = create_table_sql_with_dialect(&MySql, <CiUser as Model>::SCHEMA);
    // max_length = 200 → VARCHAR(200) on MySQL.
    assert!(
        sql.contains("`email` VARCHAR(200) COLLATE utf8mb4_general_ci"),
        "expected VARCHAR COLLATE on MySQL, got: {sql}"
    );
}

#[test]
fn postgres_extension_prelude_is_available() {
    assert_eq!(
        Postgres.ci_text_extension_sql(),
        Some("CREATE EXTENSION IF NOT EXISTS citext SCHEMA public;"),
    );
    // SQLite + MySQL need no prelude.
    assert_eq!(Sqlite.ci_text_extension_sql(), None);
    assert_eq!(MySql.ci_text_extension_sql(), None);
}

/// A fresh PG database, its pool and an admin pool to drop it with.
#[cfg(feature = "postgres")]
async fn fresh_pg(tag: &str) -> Option<(rustango::sql::Pool, rustango::sql::Pool, String)> {
    use rustango::sql::{raw_execute_pool, Pool};
    let url = std::env::var("DATABASE_URL").ok()?;
    let admin = Pool::connect(&url).await.unwrap();
    let db = format!("rustango_citext_{tag}_{}", std::process::id());
    let _ = raw_execute_pool(&admin, &format!("DROP DATABASE IF EXISTS {db}"), Vec::new()).await;
    raw_execute_pool(&admin, &format!("CREATE DATABASE {db}"), Vec::new())
        .await
        .unwrap();
    let base = url.rsplit_once('/').unwrap().0;
    let pool = Pool::connect(&format!("{base}/{db}")).await.unwrap();
    Some((admin, pool, db))
}

/// `email` compares ignoring case in the fresh database.
#[cfg(feature = "postgres")]
async fn assert_citext_works(admin: rustango::sql::Pool, pool: rustango::sql::Pool, db: String) {
    use rustango::sql::raw_execute_pool;
    raw_execute_pool(
        &pool,
        "INSERT INTO ci_users (id, email, bio) VALUES (1, 'A@x.com', '')",
        Vec::new(),
    )
    .await
    .unwrap();
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(
        "SELECT id FROM ci_users WHERE email = 'a@X.COM'",
        Vec::new(),
        &pool,
    )
    .await
    .unwrap();
    assert_eq!(got, [(1,)]);
    pool.close().await;
    let _ = raw_execute_pool(
        &admin,
        &format!("DROP DATABASE {db} WITH (FORCE)"),
        Vec::new(),
    )
    .await;
}

/// The bootstrap paths create `citext` too, not only file migrations (#2271).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn testkit_tables_create_citext_on_a_fresh_database() {
    let Some((admin, pool, db)) = fresh_pg("testkit").await else {
        return;
    };
    rustango::testkit::create_tables_for::<CiUser>(&pool)
        .await
        .expect("CITEXT on a database without the extension");
    assert_citext_works(admin, pool, db).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn apply_all_creates_citext_on_a_fresh_database() {
    let Some((admin, pool, db)) = fresh_pg("apply_all").await else {
        return;
    };
    rustango::migrate::apply_all_pool(&pool)
        .await
        .expect("CITEXT on a database without the extension");
    assert_citext_works(admin, pool, db).await;
}
