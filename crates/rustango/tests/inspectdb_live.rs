#![cfg(feature = "manage")]
//! Live test for `manage inspectdb` (roadmap #1, v0.30.13).
//!
//! Creates a fixture table set in the test DB, runs the inspectdb
//! verb against it, asserts the emitted source code includes the
//! right `#[derive(Model)]` blocks with correct attributes.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.
//!
//! Run: `DATABASE_URL=... cargo test --test inspectdb_live -- --test-threads=1`

use rustango::sql::sqlx;

use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets shared tables (via
/// DROP/CREATE or `drop_all`); under cargo's default parallel harness
/// two tests would race on PG's `pg_type_typname_nsp_index` /
/// `pg_class_relname_nsp_index` system-catalog uniques when both try
/// to CREATE/DROP at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

async fn fresh_fixture(pool: &sqlx::PgPool) {
    // Drop in dependency order so the FK doesn't block.
    for tbl in ["inspectdb_post", "inspectdb_author"] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {tbl} CASCADE"))
            .execute(pool)
            .await
            .unwrap();
    }
    // Author: BIGSERIAL PK + varchar(80) name + nullable bio +
    // default-now created_at. Exercises Auto<i64>, max_length,
    // Option<T>, and DEFAULT.
    sqlx::query(
        r#"CREATE TABLE inspectdb_author (
            id         BIGSERIAL PRIMARY KEY,
            name       VARCHAR(80) NOT NULL,
            bio        TEXT NULL,
            joined_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            status     VARCHAR(20) NOT NULL DEFAULT 'pending'
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    // Post: FK to author + uuid + jsonb + bool. Exercises FK
    // attribute, Uuid, JSON value, and bool.
    sqlx::query(
        r#"CREATE TABLE inspectdb_post (
            id           BIGSERIAL PRIMARY KEY,
            author_id    BIGINT NOT NULL REFERENCES inspectdb_author(id),
            slug         VARCHAR(120) NOT NULL,
            published    BOOLEAN NOT NULL DEFAULT FALSE,
            metadata     JSONB NULL,
            external_id  UUID NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Run `inspectdb` end-to-end and assert the emitted code carries
/// every attribute we expect for the fixture's columns. Goes
/// through `migrate::manage::run` (the same dispatcher the CLI
/// uses) rather than calling the inspectdb function directly, so
/// the verb wiring is validated too.
#[tokio::test]
async fn inspectdb_emits_models_for_fixture_tables() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else { return };
    fresh_fixture(&pool).await;

    let mut buf: Vec<u8> = Vec::new();
    let pool_enum: rustango::sql::Pool = pool.clone().into();
    rustango::migrate::manage::run_with_writer(
        &pool_enum,
        std::path::Path::new("/tmp/_inspectdb_unused"),
        vec![
            "inspectdb".into(),
            "--table".into(),
            "inspectdb_author".into(),
        ],
        &mut buf,
    )
    .await
    .expect("inspectdb verb succeeds");
    let out = String::from_utf8(buf).unwrap();

    // Header + struct shape.
    assert!(
        out.contains("Auto-emitted by `manage inspectdb`"),
        "missing header, got: {out}"
    );
    assert!(
        out.contains("pub struct InspectdbAuthor"),
        "PascalCased struct name missing, got: {out}"
    );
    assert!(
        out.contains(r#"#[rustango(table = "inspectdb_author")]"#),
        "table attr missing, got: {out}"
    );

    // Column-by-column attributes.
    assert!(
        out.contains("pub id: Auto<i64>"),
        "BIGSERIAL PK should map to Auto<i64>, got: {out}"
    );
    assert!(out.contains("primary_key"), "PK attr missing");
    assert!(
        out.contains("max_length = 80"),
        "varchar(80) max_length missing, got: {out}"
    );
    assert!(
        out.contains("pub name: String"),
        "name field shape wrong, got: {out}"
    );
    assert!(
        out.contains("pub bio: Option<String>"),
        "nullable text → Option<String>, got: {out}"
    );
    assert!(
        out.contains("pub joined_at: chrono::DateTime<chrono::Utc>"),
        "timestamptz mapping wrong, got: {out}"
    );
    assert!(
        out.contains(r#"default = "'pending'""#),
        "varchar default should be echoed (typecast stripped), got: {out}"
    );
    // nextval default should NOT appear (implied by Auto<T>).
    assert!(
        !out.contains("nextval"),
        "nextval default leaked through, got: {out}"
    );
}

/// inspectdb on the FK-bearing table emits `fk = "..."` on the
/// FK column + maps uuid + jsonb correctly.
#[tokio::test]
async fn inspectdb_emits_fk_uuid_and_jsonb_correctly() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else { return };
    fresh_fixture(&pool).await;

    let mut buf: Vec<u8> = Vec::new();
    let pool_enum: rustango::sql::Pool = pool.clone().into();
    rustango::migrate::manage::run_with_writer(
        &pool_enum,
        std::path::Path::new("/tmp/_inspectdb_unused"),
        vec![
            "inspectdb".into(),
            "--table".into(),
            "inspectdb_post".into(),
        ],
        &mut buf,
    )
    .await
    .expect("inspectdb verb succeeds");
    let out = String::from_utf8(buf).unwrap();

    assert!(
        out.contains(r#"fk = "inspectdb_author""#),
        "FK attr missing, got: {out}"
    );
    assert!(
        out.contains("pub author_id: i64"),
        "FK col is non-null bigint → i64, got: {out}"
    );
    assert!(
        out.contains("pub published: bool"),
        "bool not null mapped wrong, got: {out}"
    );
    assert!(
        out.contains("pub metadata: Option<serde_json::Value>"),
        "nullable jsonb → Option<Value>, got: {out}"
    );
    assert!(
        out.contains("pub external_id: Option<uuid::Uuid>"),
        "nullable uuid → Option<Uuid>, got: {out}"
    );
}

/// `--schema` arg gets routed through; an empty/unknown schema
/// returns the "no tables found" comment without crashing.
#[tokio::test]
async fn inspectdb_unknown_schema_emits_friendly_comment() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else { return };

    let mut buf: Vec<u8> = Vec::new();
    let pool_enum: rustango::sql::Pool = pool.clone().into();
    rustango::migrate::manage::run_with_writer(
        &pool_enum,
        std::path::Path::new("/tmp/_inspectdb_unused"),
        vec![
            "inspectdb".into(),
            "--schema".into(),
            "no_such_schema_inspect".into(),
        ],
        &mut buf,
    )
    .await
    .expect("inspectdb succeeds even on empty schema");
    let out = String::from_utf8(buf).unwrap();
    assert!(
        out.contains("no tables found"),
        "expected friendly empty-schema comment, got: {out}"
    );
}
