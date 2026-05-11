//! Live regression for v0.35 slice 1 — `M2MManager::*_pool` methods
//! against a SQLite junction table. Proves all six CRUD operations
//! (`all`, `add`, `remove`, `set`, `clear`, `contains`) work
//! end-to-end on sqlite without any Postgres dependency.
//!
//! Pairs with the existing PG-side m2m tests (gated on `DATABASE_URL`)
//! to confirm the tri-dialect rewrite preserves semantics across
//! backends.

#![cfg(feature = "sqlite")]

use rustango::core::SqlValue;
use rustango::sql::{sqlx, M2MManager, Pool};

async fn sqlite_pool_with_junction() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    // Bootstrap a minimal junction table mirroring the shape the
    // macro-emitted `<name>_m2m()` accessor expects: two i64 FK
    // columns with a UNIQUE constraint on the pair so `add`'s
    // `ON CONFLICT DO NOTHING` (sqlite supports it ≥ 3.24) has
    // something to conflict against.
    sqlx::query(
        r#"CREATE TABLE post_tags (
            post_id INTEGER NOT NULL,
            tag_id  INTEGER NOT NULL,
            UNIQUE(post_id, tag_id)
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create junction");
    Pool::Sqlite(pool)
}

fn mgr(post_id: i64) -> M2MManager {
    M2MManager {
        src_pk: SqlValue::I64(post_id),
        through: "post_tags",
        src_col: "post_id",
        dst_col: "tag_id",
    }
}

#[tokio::test]
async fn m2m_full_lifecycle_on_sqlite() {
    let pool = sqlite_pool_with_junction().await;
    let m = mgr(1);

    // Empty to start.
    let initial: Vec<i64> = m.all_pool(&pool).await.expect("all");
    assert!(initial.is_empty());
    assert!(!m.contains_pool(42, &pool).await.expect("contains"));

    // Add three.
    m.add_pool(10, &pool).await.expect("add 10");
    m.add_pool(20, &pool).await.expect("add 20");
    m.add_pool(30, &pool).await.expect("add 30");

    // add() is idempotent — duplicate is a no-op via
    // `INSERT … ON CONFLICT DO NOTHING` on sqlite.
    m.add_pool(20, &pool).await.expect("add 20 again");

    let mut got = m.all_pool(&pool).await.expect("all after add");
    got.sort_unstable();
    assert_eq!(got, vec![10, 20, 30]);

    assert!(m.contains_pool(10, &pool).await.expect("contains 10"));
    assert!(m.contains_pool(20, &pool).await.expect("contains 20"));
    assert!(!m.contains_pool(99, &pool).await.expect("contains 99"));

    // Remove one.
    m.remove_pool(20, &pool).await.expect("remove 20");
    let mut got = m.all_pool(&pool).await.expect("all after remove");
    got.sort_unstable();
    assert_eq!(got, vec![10, 30]);
    assert!(!m
        .contains_pool(20, &pool)
        .await
        .expect("contains 20 after remove"));

    // Replace via set — atomic DELETE + multi-row INSERT.
    m.set_pool(&[100, 200, 300], &pool).await.expect("set");
    let mut got = m.all_pool(&pool).await.expect("all after set");
    got.sort_unstable();
    assert_eq!(got, vec![100, 200, 300]);

    // Empty set wipes.
    m.set_pool(&[], &pool).await.expect("set empty");
    let got = m.all_pool(&pool).await.expect("all after empty set");
    assert!(got.is_empty());

    // Clear is also a wipe (idempotent).
    m.add_pool(7, &pool).await.expect("add 7");
    m.clear_pool(&pool).await.expect("clear");
    let got = m.all_pool(&pool).await.expect("all after clear");
    assert!(got.is_empty());
    m.clear_pool(&pool).await.expect("clear idempotent");
}

#[tokio::test]
async fn m2m_isolates_by_source_pk_on_sqlite() {
    let pool = sqlite_pool_with_junction().await;
    let m1 = mgr(1);
    let m2 = mgr(2);

    m1.add_pool(10, &pool).await.expect("m1 add 10");
    m1.add_pool(20, &pool).await.expect("m1 add 20");
    m2.add_pool(99, &pool).await.expect("m2 add 99");

    let mut got1 = m1.all_pool(&pool).await.expect("m1 all");
    got1.sort_unstable();
    assert_eq!(got1, vec![10, 20]);

    let got2 = m2.all_pool(&pool).await.expect("m2 all");
    assert_eq!(got2, vec![99]);

    // Clearing m1 must not touch m2.
    m1.clear_pool(&pool).await.expect("m1 clear");
    assert!(m1
        .all_pool(&pool)
        .await
        .expect("m1 all after clear")
        .is_empty());
    assert_eq!(m2.all_pool(&pool).await.expect("m2 all"), vec![99]);
}
