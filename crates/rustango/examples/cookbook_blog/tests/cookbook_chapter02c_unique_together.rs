//! Cookbook Chapter 2c — `#[rustango(unique_together = "...")]`.
//!
//! Two-column UNIQUE constraint emitted via the new container attr:
//! same DB shape as Django's `class Meta: unique_together`.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter02c_unique_together -- --test-threads=1`

use cookbook_blog::apps::blog::models::Membership;
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto};

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
            role VARCHAR(32) NOT NULL
        )"#,
    ).execute(pool).await.unwrap();
    // Mirror what `unique_together = "org_id, user_id"` lowers to in
    // the migration's CreateIndex op (Cookbook §2.18b).
    sqlx::query(
        r#"CREATE UNIQUE INDEX cookbook_membership_org_id_user_id_uq
           ON cookbook_membership (org_id, user_id)"#,
    ).execute(pool).await.unwrap();
}

// §2.18b — schema-level: Membership::SCHEMA carries one composite
// UNIQUE INDEX.
#[test]
fn unique_together_emits_composite_unique_index_in_schema() {
    let composites: Vec<_> = Membership::SCHEMA.indexes.iter().collect();
    assert_eq!(composites.len(), 1, "exactly one composite index emitted; got {composites:?}");
    let idx = &composites[0];
    assert!(idx.unique, "unique_together must be UNIQUE");
    assert_eq!(idx.columns, &["org_id", "user_id"]);
}

// §2.18b — DB rejects a second row with the same (org_id, user_id) pair.
#[tokio::test]
async fn unique_together_rejects_duplicate_pair() {
    let Some(pool) = pool().await else { return };
    fresh(&pool).await;

    let mut m1 = Membership { id: Auto::Unset, org_id: 10, user_id: 20, role: "owner".into() };
    m1.save(&pool).await.expect("first row inserts");

    // Same (10, 20) pair, different role — DB rejects.
    let mut m2 = Membership { id: Auto::Unset, org_id: 10, user_id: 20, role: "viewer".into() };
    let err = m2.save(&pool).await.expect_err("duplicate (org, user) pair must be rejected");
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        msg.contains("unique") || msg.contains("duplicate"),
        "expected UNIQUE violation; got {err:?}"
    );

    // Different pairs are fine.
    let mut m3 = Membership { id: Auto::Unset, org_id: 10, user_id: 99, role: "viewer".into() };
    m3.save(&pool).await.expect("(10, 99) is a different pair, accepted");
    let mut m4 = Membership { id: Auto::Unset, org_id: 99, user_id: 20, role: "owner".into() };
    m4.save(&pool).await.expect("(99, 20) is a different pair, accepted");
}
