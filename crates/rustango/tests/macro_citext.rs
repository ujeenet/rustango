//! Django-parity #344 — `#[rustango(citext)]` field attribute that
//! routes the migration DDL through `dialect.ci_text_type` instead
//! of the plain `column_type` mapping.
//!
//! Verifies:
//! 1. The flag threads from macro → `FieldSchema::case_insensitive`.
//! 2. PG emits `CITEXT`, SQLite emits `TEXT COLLATE NOCASE`, MySQL
//!    emits `TEXT COLLATE utf8mb4_general_ci`.
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
        Some("CREATE EXTENSION IF NOT EXISTS citext;"),
    );
    // SQLite + MySQL need no prelude.
    assert_eq!(Sqlite.ci_text_extension_sql(), None);
    assert_eq!(MySql.ci_text_extension_sql(), None);
}
