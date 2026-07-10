//! Cookbook Chapter 7e — DRF-shape `UniqueTogetherValidator`.
//!
//! The composite UNIQUE INDEX from Chapter 2c rejects duplicate
//! `(org_id, user_id)` pairs at the DB. Without a pre-check, the
//! form re-render would carry the raw Postgres error (`duplicate
//! key value violates unique constraint "..."`).
//!
//! `ModelFormFor::validate_unique_together(&pool.clone().into(), pk_value)` walks
//! every composite-unique index on the model and SELECTs the
//! conflicting tuple before the INSERT/UPDATE. Hits become friendly
//! per-field FormErrors keyed by every column in the conflict.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter07e_unique_together_validator -- --test-threads=1`

use cookbook_blog::apps::blog::models::Membership;
use rustango::forms::ModelFormFor;
use rustango::sql::{sqlx, Auto};
use std::collections::HashMap;

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }
async fn pool() -> Option<sqlx::PgPool> {
    Some(sqlx::PgPool::connect(&url()?).await.expect("connect"))
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query("DROP TABLE IF EXISTS cookbook_membership CASCADE").execute(pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE cookbook_membership (
            id BIGSERIAL PRIMARY KEY,
            org_id BIGINT NOT NULL,
            user_id BIGINT NOT NULL,
            role VARCHAR(32) NOT NULL,
            CONSTRAINT cookbook_membership_org_id_user_id_uq UNIQUE (org_id, user_id)
        )"#,
    ).execute(pool).await.unwrap();
}

fn payload(org_id: &str, user_id: &str, role: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("org_id".into(), org_id.into());
    m.insert("user_id".into(), user_id.into());
    m.insert("role".into(), role.into());
    m
}

// §7e.1 — fresh table → no conflict → validator returns Ok.
#[tokio::test]
async fn validator_accepts_when_no_existing_pair() {
    let Some(pool) = pool().await else { return };
    fresh(&pool).await;

    let mf = ModelFormFor::<Membership>::parse(&payload("10", "20", "owner")).unwrap();
    mf.validate_unique_together(&pool.clone().into(), None).await.expect("no conflict");
}

// §7e.2 — existing (org, user) → validator rejects with per-field
// FormErrors keyed by EACH column in the composite tuple.
#[tokio::test]
async fn validator_rejects_with_per_field_errors_on_create() {
    let Some(pool) = pool().await else { return };
    fresh(&pool).await;

    // Seed a row directly so the validator has something to find.
    let mut m = Membership { id: Auto::Unset, org_id: 10, user_id: 20, role: "owner".into() };
    m.save(&pool).await.unwrap();

    // Now a new form with the same (10, 20) pair must fail validation
    // BEFORE any INSERT.
    let mf = ModelFormFor::<Membership>::parse(&payload("10", "20", "viewer")).unwrap();
    let err = mf.validate_unique_together(&pool.clone().into(), None).await
        .expect_err("validator must reject the duplicate pair");
    let s = format!("{err:?}");
    // Errors are keyed by both columns — DRF-shape per-field surface.
    assert!(s.contains("\"org_id\""), "expected `org_id` in errors: {s}");
    assert!(s.contains("\"user_id\""), "expected `user_id` in errors: {s}");
    assert!(s.to_lowercase().contains("already exists"),
        "expected friendly 'already exists' message: {s}");
}

// §7e.3 — different pair = different tuple → validator passes.
#[tokio::test]
async fn validator_accepts_different_pair() {
    let Some(pool) = pool().await else { return };
    fresh(&pool).await;

    let mut m = Membership { id: Auto::Unset, org_id: 10, user_id: 20, role: "owner".into() };
    m.save(&pool).await.unwrap();

    // (10, 99) is a different tuple — ok.
    let mf = ModelFormFor::<Membership>::parse(&payload("10", "99", "viewer")).unwrap();
    mf.validate_unique_together(&pool.clone().into(), None).await.expect("(10,99) is unique vs (10,20)");
    // (99, 20) likewise.
    let mf = ModelFormFor::<Membership>::parse(&payload("99", "20", "viewer")).unwrap();
    mf.validate_unique_together(&pool.clone().into(), None).await.expect("(99,20) is unique vs (10,20)");
}

// §7e.4 — UPDATE: pass the row's own PK so its existing tuple isn't
// counted as a conflict against itself.
#[tokio::test]
async fn validator_excludes_own_row_on_update() {
    use rustango::core::SqlValue;
    let Some(pool) = pool().await else { return };
    fresh(&pool).await;

    let mut m = Membership { id: Auto::Unset, org_id: 10, user_id: 20, role: "owner".into() };
    m.save(&pool).await.unwrap();
    let pk = match m.id { Auto::Set(v) => v, _ => unreachable!() };

    // UPDATE that "changes" the role but keeps (org, user) — without
    // pk_value the validator would reject (the row's own tuple is its
    // own conflict). Passing pk_value skips it.
    let mf = ModelFormFor::<Membership>::parse(&payload("10", "20", "admin")).unwrap();
    mf.validate_unique_together(&pool.clone().into(), Some(&SqlValue::I64(pk))).await
        .expect("own row excluded — UPDATE is fine");

    // But if we set a DIFFERENT row's tuple, validator still rejects.
    let mut m2 = Membership { id: Auto::Unset, org_id: 11, user_id: 21, role: "viewer".into() };
    m2.save(&pool).await.unwrap();
    let pk2 = match m2.id { Auto::Set(v) => v, _ => unreachable!() };

    // Try to UPDATE row #2 to row #1's tuple → conflict.
    let mf = ModelFormFor::<Membership>::parse(&payload("10", "20", "viewer")).unwrap();
    mf.validate_unique_together(&pool.clone().into(), Some(&SqlValue::I64(pk2))).await
        .expect_err("conflicting pair on a different row must reject");
}
