//! Issue #437 — the unique-together serializer validator. Pre-save
//! check that a candidate row doesn't collide on any of the model's
//! declared `unique_together` constraints. Tri-dialect since #1872.

#![cfg(all(
    feature = "serializer",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::collections::HashMap;

use rustango::core::{Model as _, SqlValue};
use rustango::forms::ModelFormFor;
use rustango::serializer::check_unique_together_pool;
use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "utv_membership", unique_together = "org_id, user_id")]
pub struct UtvMembership {
    #[rustango(primary_key)]
    id: Auto<i64>,
    org_id: i64,
    user_id: i64,
}

/// Unique only where `user_id > 100`; lower ids may repeat.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "utv_partial",
    unique_when(columns = "org_id, user_id", condition = "user_id > 100")
)]
pub struct UtvPartial {
    #[rustango(primary_key)]
    id: Auto<i64>,
    org_id: i64,
    user_id: i64,
}

/// A nullable column in the pair: NULLs never collide in a unique index.
#[derive(Model, Debug, Clone)]
#[rustango(table = "utv_nullable", unique_together = "org_id, user_id")]
pub struct UtvNullable {
    #[rustango(primary_key)]
    id: Auto<i64>,
    org_id: i64,
    user_id: Option<i64>,
}

fn form<T: rustango::core::Model>(org_id: i64, user_id: i64) -> ModelFormFor<T> {
    ModelFormFor::<T>::from_json(&serde_json::json!({ "org_id": org_id, "user_id": user_id }))
        .expect("form parses")
}

async fn seed(pool: &Pool, org_id: i64, user_id: i64) -> i64 {
    let mut m = UtvMembership {
        id: Auto::Unset,
        org_id,
        user_id,
    };
    m.save_pool(pool).await.expect("seed");
    *m.id.get().expect("pk")
}

fn values(org_id: i64, user_id: i64) -> HashMap<&'static str, SqlValue> {
    let mut m = HashMap::new();
    m.insert("org_id", SqlValue::I64(org_id));
    m.insert("user_id", SqlValue::I64(user_id));
    m
}

async fn validator_returns_ok_when_no_collision(pool: &Pool) {
    seed(pool, 1, 2).await;
    check_unique_together_pool(pool, UtvMembership::SCHEMA, &values(1, 99), None)
        .await
        .expect("non-colliding pair should be accepted");
}

async fn validator_returns_err_on_collision(pool: &Pool) {
    seed(pool, 1, 2).await;
    let err = check_unique_together_pool(pool, UtvMembership::SCHEMA, &values(1, 2), None)
        .await
        .unwrap_err();
    // #1872: on Postgres the probe failed to decode, so this said
    // "unique_together check failed" instead.
    assert_eq!(
        err.non_field(),
        ["The fields org_id, user_id must be unique together."],
        "{err:?}"
    );
}

async fn exclude_pk_lets_a_row_re_save_its_own_values(pool: &Pool) {
    let pk = seed(pool, 1, 2).await;
    check_unique_together_pool(
        pool,
        UtvMembership::SCHEMA,
        &values(1, 2),
        Some(&SqlValue::I64(pk)),
    )
    .await
    .expect("self-update should not flag a collision");
}

async fn exclude_pk_still_catches_collisions_against_other_rows(pool: &Pool) {
    let pk = seed(pool, 1, 2).await;
    seed(pool, 2, 3).await;
    let err = check_unique_together_pool(
        pool,
        UtvMembership::SCHEMA,
        &values(2, 3),
        Some(&SqlValue::I64(pk)),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.non_field(),
        ["The fields org_id, user_id must be unique together."],
        "{err:?}"
    );
}

async fn partial_value_set_skips_the_check(pool: &Pool) {
    // Only org_id is bound: not enough to find a collision, so skip.
    seed(pool, 1, 2).await;
    let mut partial = HashMap::new();
    partial.insert("org_id", SqlValue::I64(1));
    check_unique_together_pool(pool, UtvMembership::SCHEMA, &partial, None)
        .await
        .expect("partial bind should be a silent skip, not an error");
}

async fn model_form_flags_a_collision(pool: &Pool) {
    let pk = seed(pool, 1, 2).await;
    let err = form::<UtvMembership>(1, 2)
        .validate_unique_together(pool, None)
        .await
        .unwrap_err();
    assert!(
        err.non_field().is_empty(),
        "the probe itself failed: {err:?}"
    );
    assert_eq!(
        err.get("org_id"),
        ["a row with the same (org_id, user_id) already exists"]
    );
    form::<UtvMembership>(1, 2)
        .validate_unique_together(pool, Some(&SqlValue::I64(pk)))
        .await
        .expect("own row is not a conflict");
    form::<UtvMembership>(1, 3)
        .validate_unique_together(pool, None)
        .await
        .expect("free pair");
}

async fn model_form_skips_a_partial_unique_index(pool: &Pool) {
    // #2011: a repeat outside the index predicate is legal.
    rustango::testkit::matrix::fresh_table::<UtvPartial>(pool).await;
    let mut row = UtvPartial {
        id: Auto::Unset,
        org_id: 1,
        user_id: 2,
    };
    row.save_pool(pool).await.expect("seed");
    form::<UtvPartial>(1, 2)
        .validate_unique_together(pool, None)
        .await
        .expect("partial index does not cover this row");
}

/// #2120: `fresh_table` built no `unique_together` index, so this passed.
async fn the_table_itself_rejects_a_duplicate_pair(pool: &Pool) {
    seed(pool, 1, 2).await;
    let mut dup = UtvMembership {
        id: Auto::Unset,
        org_id: 1,
        user_id: 2,
    };
    assert!(
        dup.save_pool(pool).await.is_err(),
        "duplicate pair inserted"
    );
}

/// `None` now writes `IS NULL` (#2413); a NULL pair must still pass.
async fn a_null_in_the_pair_never_collides(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<UtvNullable>(pool).await;
    let mut row = UtvNullable {
        id: Auto::Unset,
        org_id: 1,
        user_id: None,
    };
    row.save_pool(pool).await.expect("seed");
    let mut pair = HashMap::new();
    pair.insert("org_id", SqlValue::I64(1));
    pair.insert("user_id", SqlValue::Null);
    check_unique_together_pool(pool, UtvNullable::SCHEMA, &pair, None)
        .await
        .expect("serializer check: NULL is no collision");
    ModelFormFor::<UtvNullable>::from_json(&serde_json::json!({ "org_id": 1, "user_id": null }))
        .expect("form parses")
        .validate_unique_together(pool, None)
        .await
        .expect("form check: NULL is no collision");
}

tri_dialect_test!(
    model: UtvMembership,
    scenarios: [
        the_table_itself_rejects_a_duplicate_pair,
        validator_returns_ok_when_no_collision,
        validator_returns_err_on_collision,
        exclude_pk_lets_a_row_re_save_its_own_values,
        exclude_pk_still_catches_collisions_against_other_rows,
        partial_value_set_skips_the_check,
        model_form_flags_a_collision,
        model_form_skips_a_partial_unique_index,
        a_null_in_the_pair_never_collides,
    ],
);
