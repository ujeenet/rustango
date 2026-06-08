#![cfg(feature = "sqlite")]
//! Regression: a model that declares fields colliding with the
//! Eloquent-style inherent shortcuts (`count`, `first`, …) must still
//! compile **and** keep the always-emitted helpers that internally
//! relied on those shortcuts working.
//!
//! The 2026-06-07 field/shortcut collision guard suppresses the
//! conflicting shortcut (e.g. `Model::count`) when a same-named field
//! exists — but `Model::paginate` / `first_or_fail` / `first_or` were
//! still emitting `Self::count(...)` / `Self::first(...)` calls, which
//! then resolved to the per-field column const → E0618 "expected
//! function, found <field>_col". This pins the fix: those helpers route
//! through `QuerySet` directly, independent of the guarded methods.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fsc_widget")]
#[rustango(app = "field_shortcut_collision")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    // `count` collides with the generated `Widget::count()` shortcut;
    // `first` collides with `Widget::first()`. Both shortcuts are
    // suppressed — the always-emitted helpers must not reference them.
    pub count: i32,
    pub first: i32,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE fsc_widget (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            count INTEGER NOT NULL,
            first INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn insert(pool: &Pool, count: i32, first: i32) {
    let mut w = Widget {
        id: Auto::default(),
        count,
        first,
    };
    w.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn paginate_compiles_and_counts_with_count_field() {
    let pool = make_pool().await;
    insert(&pool, 1, 10).await;
    insert(&pool, 2, 20).await;
    insert(&pool, 3, 30).await;
    // `paginate` internally counts the table — must route through the
    // queryset, not the suppressed `Widget::count` method.
    let (rows, total) = Widget::paginate(1, 2, &pool).await.unwrap();
    assert_eq!(total, 3, "total row count");
    assert_eq!(rows.len(), 2, "first page of 2");
}

#[tokio::test]
async fn first_or_fail_compiles_with_first_field() {
    let pool = make_pool().await;
    insert(&pool, 7, 70).await;
    let row = Widget::first_or_fail(&pool).await.unwrap();
    assert_eq!(row.count, 7);
}

#[tokio::test]
async fn first_or_fail_errors_on_empty_table() {
    let pool = make_pool().await;
    let err = Widget::first_or_fail(&pool).await;
    assert!(err.is_err(), "empty table → RowNotFound");
}

#[tokio::test]
async fn first_or_compiles_and_falls_back_with_first_field() {
    let pool = make_pool().await;
    // Empty table → fallback closure supplies the row.
    let row = Widget::first_or(&pool, || Widget {
        id: Auto::default(),
        count: -1,
        first: -1,
    })
    .await
    .unwrap();
    assert_eq!(row.count, -1, "fallback used on empty table");

    insert(&pool, 5, 50).await;
    let row = Widget::first_or(&pool, || Widget {
        id: Auto::default(),
        count: -1,
        first: -1,
    })
    .await
    .unwrap();
    assert_eq!(row.count, 5, "existing row preferred over fallback");
}
