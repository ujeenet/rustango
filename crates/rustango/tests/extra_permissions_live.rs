//! Django parity — `Meta.permissions = [(codename, name), ...]`
//! lets a model declare custom authorization buckets alongside the
//! auto-generated CRUD codenames. rustango spells the attribute as
//! `#[rustango(extra_permissions = "codename:label, codename:label")]`
//! and `auto_create_permissions_pool` seeds the
//! `rustango_permissions` table with one row per pair.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::permissions::auto_create_permissions_pool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "xperms_post",
    permissions,
    extra_permissions = "approve:Can approve posts, archive:Can archive posts"
)]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "xperms_plain", permissions)]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    Pool::Sqlite(pool)
}

#[test]
fn schema_carries_extra_permission_tuples() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    assert_eq!(
        schema.extra_permissions,
        &[
            ("approve", "Can approve posts"),
            ("archive", "Can archive posts"),
        ]
    );
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(plain.extra_permissions.is_empty());
}

#[tokio::test]
async fn auto_create_permissions_seeds_extra_codenames() {
    let pool = fresh_pool().await;
    rustango::testkit::migrate_framework(&pool).await.unwrap();
    auto_create_permissions_pool(&pool).await.unwrap();

    // Verify the extra codenames landed under the model's table.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT table_name, codename, name FROM rustango_permissions \
         WHERE table_name = 'xperms_post' ORDER BY codename",
    )
    .fetch_all(sq)
    .await
    .unwrap();

    // Per-model: 4 CRUD codenames + 2 extras = 6 rows.
    assert_eq!(rows.len(), 6, "got: {rows:?}");

    // Verify the two extras specifically.
    let approve = rows.iter().find(|(_, c, _)| c == "xperms_post.approve");
    let archive = rows.iter().find(|(_, c, _)| c == "xperms_post.archive");
    assert!(approve.is_some(), "missing xperms_post.approve: {rows:?}");
    assert!(archive.is_some(), "missing xperms_post.archive: {rows:?}");
    assert_eq!(approve.unwrap().2, "Can approve posts");
    assert_eq!(archive.unwrap().2, "Can archive posts");
}

#[tokio::test]
async fn idempotent_re_seed_doesnt_duplicate_extras() {
    let pool = fresh_pool().await;
    rustango::testkit::migrate_framework(&pool).await.unwrap();
    auto_create_permissions_pool(&pool).await.unwrap();
    // Second call should be a no-op via the existing ON CONFLICT DO NOTHING
    // tail in the INSERT — both extras + CRUD codenames stay at one row each.
    auto_create_permissions_pool(&pool).await.unwrap();
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM rustango_permissions WHERE table_name = 'xperms_post'",
    )
    .fetch_one(sq)
    .await
    .unwrap();
    assert_eq!(count.0, 6, "double-seed must stay idempotent");
}

#[tokio::test]
async fn plain_model_seeds_only_crud_codenames() {
    let pool = fresh_pool().await;
    rustango::testkit::migrate_framework(&pool).await.unwrap();
    auto_create_permissions_pool(&pool).await.unwrap();
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM rustango_permissions WHERE table_name = 'xperms_plain'",
    )
    .fetch_one(sq)
    .await
    .unwrap();
    assert_eq!(count.0, 4, "no extra_permissions → only CRUD 4 codenames");
}
