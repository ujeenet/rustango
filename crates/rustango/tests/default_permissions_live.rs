//! Django parity — `Meta.default_permissions = ('view', 'change')`
//! lets a model opt out of the default `add` / `delete` CRUD
//! codenames. rustango spells the attribute as
//! `#[rustango(default_permissions = "view,change")]` and
//! `auto_create_permissions_pool` filters the four-action seed loop
//! against the declared subset.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::permissions::auto_create_permissions_pool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "dp_readonly", permissions, default_permissions = "view")]
#[allow(dead_code)]
pub struct ReadOnly {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "dp_no_delete",
    permissions,
    default_permissions = "add,change,view"
)]
#[allow(dead_code)]
pub struct NoDelete {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "dp_all_four", permissions)]
#[allow(dead_code)]
pub struct AllFour {
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
fn schema_carries_default_permissions_when_set() {
    let read_only = <ReadOnly as rustango::core::Model>::SCHEMA;
    assert_eq!(read_only.default_permissions, &["view"]);

    let no_delete = <NoDelete as rustango::core::Model>::SCHEMA;
    assert_eq!(no_delete.default_permissions, &["add", "change", "view"]);

    let all_four = <AllFour as rustango::core::Model>::SCHEMA;
    assert!(
        all_four.default_permissions.is_empty(),
        "no attr → empty slice → seeder uses all four CRUD codenames"
    );
}

#[tokio::test]
async fn seeder_emits_only_declared_subset() {
    let pool = fresh_pool().await;
    rustango::testkit::migrate_framework(&pool).await.unwrap();
    auto_create_permissions_pool(&pool).await.unwrap();

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };

    // dp_readonly: view only — 1 codename.
    let read_only_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT codename FROM rustango_permissions WHERE table_name = 'dp_readonly' \
         ORDER BY codename",
    )
    .fetch_all(sq)
    .await
    .unwrap();
    let read_only: Vec<String> = read_only_rows.into_iter().map(|(c,)| c).collect();
    assert_eq!(read_only, vec!["dp_readonly.view".to_owned()]);

    // dp_no_delete: add + change + view — 3 codenames (NO delete).
    let no_delete_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT codename FROM rustango_permissions WHERE table_name = 'dp_no_delete' \
         ORDER BY codename",
    )
    .fetch_all(sq)
    .await
    .unwrap();
    let no_delete: Vec<String> = no_delete_rows.into_iter().map(|(c,)| c).collect();
    assert_eq!(
        no_delete,
        vec![
            "dp_no_delete.add".to_owned(),
            "dp_no_delete.change".to_owned(),
            "dp_no_delete.view".to_owned(),
        ],
        "delete must be skipped"
    );

    // dp_all_four: no attr → all 4 codenames.
    let all_four_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT codename FROM rustango_permissions WHERE table_name = 'dp_all_four' \
         ORDER BY codename",
    )
    .fetch_all(sq)
    .await
    .unwrap();
    let all_four: Vec<String> = all_four_rows.into_iter().map(|(c,)| c).collect();
    assert_eq!(
        all_four,
        vec![
            "dp_all_four.add".to_owned(),
            "dp_all_four.change".to_owned(),
            "dp_all_four.delete".to_owned(),
            "dp_all_four.view".to_owned(),
        ],
        "default-empty must seed all four CRUD codenames"
    );
}

#[tokio::test]
async fn re_seed_stays_idempotent_with_filtered_set() {
    let pool = fresh_pool().await;
    rustango::testkit::migrate_framework(&pool).await.unwrap();
    auto_create_permissions_pool(&pool).await.unwrap();
    auto_create_permissions_pool(&pool).await.unwrap();

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM rustango_permissions WHERE table_name = 'dp_readonly'",
    )
    .fetch_one(sq)
    .await
    .unwrap();
    assert_eq!(count, 1, "double-seed must stay idempotent");
}
