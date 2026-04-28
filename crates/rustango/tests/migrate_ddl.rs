//! Unit tests for the Postgres DDL writer in `rustango-migrate`.
//!
//! Each model exercises a different combination of types, bounds, and
//! relations. Live migration apply/drop is in [`migrate_live`](./migrate_live.rs).

use rustango::core::Model as _;
use rustango::migrate::ddl::{
    create_constraints_sql, create_table_if_not_exists_sql, create_table_sql, drop_table_sql,
};
use rustango::Model;

// ---------------- scalar types ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_scalars")]
pub struct Scalars {
    #[rustango(primary_key)]
    id: i64,
    little: i32,
    flt32: f32,
    flt64: f64,
    flag: bool,
    note: String,
    when: chrono::DateTime<chrono::Utc>,
    day: chrono::NaiveDate,
    handle: uuid::Uuid,
    payload: serde_json::Value,
}

#[test]
fn create_table_maps_each_field_type_to_pg_type() {
    let sql = create_table_sql(Scalars::SCHEMA);
    assert_eq!(
        sql,
        r#"CREATE TABLE "ddl_scalars" ("id" BIGINT NOT NULL PRIMARY KEY, "little" INTEGER NOT NULL, "flt32" REAL NOT NULL, "flt64" DOUBLE PRECISION NOT NULL, "flag" BOOLEAN NOT NULL, "note" TEXT NOT NULL, "when" TIMESTAMPTZ NOT NULL, "day" DATE NOT NULL, "handle" UUID NOT NULL, "payload" JSONB NOT NULL)"#,
    );
}

// ---------------- nullability ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_nullable")]
pub struct Nullable {
    #[rustango(primary_key)]
    id: i64,
    name: Option<String>,
    age: Option<i32>,
}

#[test]
fn option_fields_omit_not_null() {
    let sql = create_table_sql(Nullable::SCHEMA);
    assert_eq!(
        sql,
        r#"CREATE TABLE "ddl_nullable" ("id" BIGINT NOT NULL PRIMARY KEY, "name" TEXT, "age" INTEGER)"#,
    );
}

// ---------------- bounds ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_bounded")]
pub struct Bounded {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
    #[rustango(max_length = 64)]
    nickname: Option<String>,
    #[rustango(min = 0, max = 150)]
    age: i32,
    #[rustango(min = -100)]
    score: i64,
    #[rustango(max = 100)]
    cap: i32,
}

#[test]
fn max_length_emits_varchar() {
    let sql = create_table_sql(Bounded::SCHEMA);
    assert!(sql.contains(r#""name" VARCHAR(32) NOT NULL"#), "{sql}");
    assert!(sql.contains(r#""nickname" VARCHAR(64),"#), "{sql}");
}

#[test]
fn min_and_max_emit_check_constraint() {
    let sql = create_table_sql(Bounded::SCHEMA);
    assert!(
        sql.contains(r#""age" INTEGER NOT NULL CHECK ("age" >= 0 AND "age" <= 150)"#),
        "{sql}",
    );
}

#[test]
fn only_min_emits_single_sided_check() {
    let sql = create_table_sql(Bounded::SCHEMA);
    assert!(
        sql.contains(r#""score" BIGINT NOT NULL CHECK ("score" >= -100)"#),
        "{sql}",
    );
}

#[test]
fn only_max_emits_single_sided_check() {
    let sql = create_table_sql(Bounded::SCHEMA);
    assert!(
        sql.contains(r#""cap" INTEGER NOT NULL CHECK ("cap" <= 100)"#),
        "{sql}",
    );
}

// ---------------- defaults ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_defaults")]
pub struct Defaults {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(default = "0")]
    score: i32,
    #[rustango(max_length = 16, default = "'draft'")]
    status: String,
    #[rustango(default = "true")]
    is_active: bool,
    #[rustango(default = "NOW()")]
    created: chrono::DateTime<chrono::Utc>,
    nickname: Option<String>,
}

#[test]
fn default_emits_after_type_before_not_null() {
    let sql = create_table_sql(Defaults::SCHEMA);
    assert!(
        sql.contains(r#""score" INTEGER DEFAULT 0 NOT NULL"#),
        "{sql}",
    );
    assert!(
        sql.contains(r#""status" VARCHAR(16) DEFAULT 'draft' NOT NULL"#),
        "{sql}",
    );
    assert!(
        sql.contains(r#""is_active" BOOLEAN DEFAULT true NOT NULL"#),
        "{sql}",
    );
    assert!(
        sql.contains(r#""created" TIMESTAMPTZ DEFAULT NOW() NOT NULL"#),
        "{sql}",
    );
    // Untouched field has no DEFAULT clause.
    assert!(sql.contains(r#""nickname" TEXT"#), "{sql}");
    assert!(!sql.contains(r#""nickname" TEXT DEFAULT"#), "{sql}");
}

// ---------------- foreign keys ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_post")]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    title: String,
    #[rustango(fk = "ddl_user", on = "id")]
    author_id: i64,
}

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_profile")]
pub struct Profile {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(o2o = "ddl_user")]
    user_id: i64,
}

#[test]
fn fk_columns_appear_inline_without_constraint() {
    // FKs should be plain columns in CREATE TABLE; constraint is separate.
    let sql = create_table_sql(Post::SCHEMA);
    assert!(sql.contains(r#""author_id" BIGINT NOT NULL"#), "{sql}");
    assert!(!sql.contains("REFERENCES"), "{sql}");
    assert!(!sql.contains("FOREIGN KEY"), "{sql}");
}

#[test]
fn fk_emits_alter_table_constraint() {
    let constraints = create_constraints_sql(Post::SCHEMA);
    assert_eq!(constraints.len(), 1);
    assert_eq!(
        constraints[0],
        r#"ALTER TABLE "ddl_post" ADD CONSTRAINT "ddl_post_author_id_fkey" FOREIGN KEY ("author_id") REFERENCES "ddl_user" ("id")"#,
    );
}

#[test]
fn o2o_uses_default_id_target() {
    let constraints = create_constraints_sql(Profile::SCHEMA);
    assert_eq!(constraints.len(), 1);
    assert_eq!(
        constraints[0],
        r#"ALTER TABLE "ddl_profile" ADD CONSTRAINT "ddl_profile_user_id_fkey" FOREIGN KEY ("user_id") REFERENCES "ddl_user" ("id")"#,
    );
}

#[test]
fn no_relation_means_no_constraints() {
    assert!(create_constraints_sql(Scalars::SCHEMA).is_empty());
    assert!(create_constraints_sql(Bounded::SCHEMA).is_empty());
}

// ---------------- DROP and IF NOT EXISTS ----------------

#[test]
fn drop_table_default_is_plain() {
    let sql = drop_table_sql(Post::SCHEMA, false, false);
    assert_eq!(sql, r#"DROP TABLE "ddl_post""#);
}

#[test]
fn drop_table_if_exists_cascade_emits_both_clauses() {
    let sql = drop_table_sql(Post::SCHEMA, true, true);
    assert_eq!(sql, r#"DROP TABLE IF EXISTS "ddl_post" CASCADE"#);
}

#[test]
fn create_table_if_not_exists_inserts_clause() {
    let sql = create_table_if_not_exists_sql(Post::SCHEMA);
    assert!(sql.starts_with(r#"CREATE TABLE IF NOT EXISTS "ddl_post" ("#));
    assert!(sql.contains(r#""title" TEXT NOT NULL"#));
}

// ---------------- identifier quoting ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_quoted")]
pub struct Quoted {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(column = "weird name")]
    weird: String,
}

#[test]
fn identifiers_are_double_quoted() {
    let sql = create_table_sql(Quoted::SCHEMA);
    assert!(sql.contains(r#""weird name" TEXT NOT NULL"#), "{sql}");
}

// ---------------- additional coverage ----------------

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_multi_fk")]
pub struct MultiFk {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(fk = "ddl_user", on = "id")]
    author_id: i64,
    #[rustango(fk = "ddl_post", on = "id")]
    post_id: i64,
}

#[test]
fn multi_fk_emits_one_constraint_per_relation() {
    let constraints = create_constraints_sql(MultiFk::SCHEMA);
    assert_eq!(constraints.len(), 2);
    assert!(constraints.iter().any(|c| c.contains("author_id_fkey")));
    assert!(constraints.iter().any(|c| c.contains("post_id_fkey")));
}

#[test]
fn drop_table_if_exists_only() {
    let sql = drop_table_sql(MultiFk::SCHEMA, true, false);
    assert_eq!(sql, r#"DROP TABLE IF EXISTS "ddl_multi_fk""#);
}

#[test]
fn drop_table_cascade_only() {
    let sql = drop_table_sql(MultiFk::SCHEMA, false, true);
    assert_eq!(sql, r#"DROP TABLE "ddl_multi_fk" CASCADE"#);
}

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_no_check")]
pub struct NoCheck {
    #[rustango(primary_key)]
    id: i64,
    untouched: i32,
}

#[test]
fn no_min_or_max_means_no_check_clause() {
    let sql = create_table_sql(NoCheck::SCHEMA);
    assert!(!sql.contains("CHECK"), "no min/max → no CHECK: {sql}");
}

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_default_nullable")]
pub struct DefaultNullable {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(default = "0")]
    score: Option<i32>,
}

#[test]
fn default_works_on_nullable_column_too() {
    let sql = create_table_sql(DefaultNullable::SCHEMA);
    // Column is nullable AND has a default — no NOT NULL, but DEFAULT present.
    assert!(sql.contains(r#""score" INTEGER DEFAULT 0"#), "{sql}");
    assert!(
        !sql.contains(r#""score" INTEGER DEFAULT 0 NOT NULL"#),
        "{sql}"
    );
}

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_bool_default")]
pub struct BoolDefault {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(default = "false")]
    flag: bool,
}

#[test]
fn bool_default_emits_keyword_not_quoted() {
    let sql = create_table_sql(BoolDefault::SCHEMA);
    assert!(
        sql.contains(r#""flag" BOOLEAN DEFAULT false NOT NULL"#),
        "{sql}",
    );
}

#[derive(Model)]
#[allow(dead_code)]
#[rustango(table = "ddl_min_only")]
pub struct MinOnly {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(min = -100)]
    floor_only: i32,
}

#[test]
fn min_only_uses_signed_value_in_check() {
    let sql = create_table_sql(MinOnly::SCHEMA);
    assert!(sql.contains(r#"CHECK ("floor_only" >= -100)"#), "{sql}",);
    assert!(!sql.contains("AND"), "no AND for single-sided check: {sql}");
}
