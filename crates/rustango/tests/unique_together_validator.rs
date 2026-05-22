//! Django-parity #437 — DRF `UniqueTogetherValidator`. Pre-save
//! check that a candidate row doesn't collide on any of the model's
//! declared `unique_together` constraints.

#![cfg(all(feature = "sqlite", feature = "serializer", feature = "tenancy"))]

use std::collections::HashMap;

use rustango::core::{Model as _, SqlValue};
use rustango::serializer::check_unique_together_pool;
use rustango::sql::Pool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "utv_membership", unique_together = "org_id, user_id")]
#[allow(dead_code)]
pub struct UtvMembership {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    org_id: i64,
    user_id: i64,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "utv_membership" (
            "id"      INTEGER PRIMARY KEY AUTOINCREMENT,
            "org_id"  INTEGER NOT NULL,
            "user_id" INTEGER NOT NULL,
            UNIQUE ("org_id", "user_id")
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    // Seed one row — (org=1, user=2).
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "utv_membership" ("org_id", "user_id") VALUES (1, 2)"#,
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

fn values(org_id: i64, user_id: i64) -> HashMap<&'static str, SqlValue> {
    let mut m = HashMap::new();
    m.insert("org_id", SqlValue::I64(org_id));
    m.insert("user_id", SqlValue::I64(user_id));
    m
}

#[tokio::test]
async fn validator_returns_ok_when_no_collision() {
    let pool = build_pool().await;
    check_unique_together_pool(&pool, UtvMembership::SCHEMA, &values(1, 99), None)
        .await
        .expect("non-colliding pair should be accepted");
}

#[tokio::test]
async fn validator_returns_err_on_collision() {
    let pool = build_pool().await;
    let err = check_unique_together_pool(&pool, UtvMembership::SCHEMA, &values(1, 2), None)
        .await
        .unwrap_err();
    let msg = err.non_field().join(" | ");
    assert!(
        msg.contains("org_id")
            && msg.contains("user_id")
            && msg.contains("must be unique together"),
        "expected unique-together message, got: {msg}"
    );
}

#[tokio::test]
async fn exclude_pk_lets_a_row_re_save_its_own_values() {
    // Updating row id=1 with the same (org=1, user=2) should not
    // collide with itself — pass exclude_pk = Some(id).
    let pool = build_pool().await;
    check_unique_together_pool(
        &pool,
        UtvMembership::SCHEMA,
        &values(1, 2),
        Some(&SqlValue::I64(1)),
    )
    .await
    .expect("self-update should not flag a collision");
}

#[tokio::test]
async fn exclude_pk_still_catches_collisions_against_other_rows() {
    // Seed a second row, then try to update row 1 to look like row 2.
    let pool = build_pool().await;
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "utv_membership" ("org_id", "user_id") VALUES (2, 3)"#,
        Vec::new(),
    )
    .await
    .expect("seed second");
    let err = check_unique_together_pool(
        &pool,
        UtvMembership::SCHEMA,
        &values(2, 3),
        Some(&SqlValue::I64(1)),
    )
    .await
    .unwrap_err();
    assert!(!err.non_field().is_empty());
}

#[tokio::test]
async fn partial_value_set_skips_the_check() {
    // Only org_id provided — not enough to identify a unique-together
    // collision, so the check skips.
    let pool = build_pool().await;
    let mut partial = HashMap::new();
    partial.insert("org_id", SqlValue::I64(1));
    check_unique_together_pool(&pool, UtvMembership::SCHEMA, &partial, None)
        .await
        .expect("partial bind should be a silent skip, not an error");
}
