#![cfg(feature = "sqlite")]
//! Live SQLite test for the ergonomic `Model::bulk_update(objs, fields)`
//! API — Django's `QuerySet.bulk_update`.
//!
//! The SQL/IR/executor stack (`BulkUpdateQuery` + `bulk_update_pool` +
//! the per-dialect `write_bulk_update_*` writers) already existed and is
//! covered by `sqlite_bulk_update_live.rs` (it hand-builds the IR). This
//! test covers the *new* per-model constructor: that `&[Self]` + a
//! runtime column list lowers into the right `[pk, col_vals…]` rows,
//! writes per-row-different values, leaves unnamed columns untouched,
//! and rejects the PK / unknown columns with clear errors.
//!
//! `max_connections(1)` pins the seed + the update + the read-back to one
//! in-memory database (a multi-connection `:memory:` pool would hand each
//! query a fresh empty DB).

use rustango::core::QueryError;
use rustango::sql::{sqlx, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bua_widget")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
    pub qty: i64,
    #[rustango(max_length = 40)]
    pub note: String,
}

async fn seeded_pool() -> Pool {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query(
        "CREATE TABLE bua_widget (\
            id INTEGER PRIMARY KEY, \
            name TEXT NOT NULL, \
            qty INTEGER NOT NULL, \
            note TEXT NOT NULL)",
    )
    .execute(&p)
    .await
    .unwrap();
    for (id, name) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
        sqlx::query("INSERT INTO bua_widget (id, name, qty, note) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(name)
            .bind(id * 10)
            .bind("original")
            .execute(&p)
            .await
            .unwrap();
    }
    p.into()
}

async fn row(pool: &Pool, id: i64) -> (String, i64, String) {
    // Refutable under `--all-features` (Pool has PG/MySQL variants too);
    // irrefutable under sqlite-only — keep the arm, silence the lint.
    #[allow(irrefutable_let_patterns)]
    let Pool::Sqlite(p) = pool
    else {
        unreachable!()
    };
    sqlx::query_as::<_, (String, i64, String)>(
        "SELECT name, qty, note FROM bua_widget WHERE id = ?",
    )
    .bind(id)
    .fetch_one(p)
    .await
    .unwrap()
}

#[tokio::test]
async fn bulk_update_writes_per_row_values_and_leaves_unnamed_columns_alone() {
    let pool = seeded_pool().await;

    // Different new values per row. `note` is mutated in memory too, but
    // we DON'T name it — it must stay `"original"` in the database.
    let objs = vec![
        Widget {
            id: 1,
            name: "alpha!".into(),
            qty: 111,
            note: "should-not-persist".into(),
        },
        Widget {
            id: 2,
            name: "beta!".into(),
            qty: 222,
            note: "should-not-persist".into(),
        },
        Widget {
            id: 3,
            name: "gamma!".into(),
            qty: 333,
            note: "should-not-persist".into(),
        },
    ];

    let affected = Widget::bulk_update(&objs, &["name", "qty"], &pool)
        .await
        .expect("bulk_update");
    assert_eq!(affected, 3, "all three rows updated in one statement");

    for (id, name, qty) in [(1, "alpha!", 111), (2, "beta!", 222), (3, "gamma!", 333)] {
        let (got_name, got_qty, got_note) = row(&pool, id).await;
        assert_eq!(got_name, name, "row {id} name");
        assert_eq!(got_qty, qty, "row {id} qty");
        assert_eq!(
            got_note, "original",
            "row {id} note must be untouched — it wasn't named in `fields`"
        );
    }
}

#[tokio::test]
async fn bulk_update_rejects_the_primary_key() {
    let pool = seeded_pool().await;
    let objs = vec![Widget {
        id: 1,
        name: "x".into(),
        qty: 1,
        note: "x".into(),
    }];

    let err = Widget::bulk_update(&objs, &["name", "id"], &pool)
        .await
        .expect_err("naming the PK must error");
    match err {
        ExecError::Query(QueryError::BulkUpdatePrimaryKey { field, .. }) => {
            assert_eq!(field, "id");
        }
        other => panic!("expected BulkUpdatePrimaryKey, got {other:?}"),
    }

    // The rejection happens before any write — the row is unchanged.
    assert_eq!(row(&pool, 1).await, ("alpha".into(), 10, "original".into()));
}

#[tokio::test]
async fn bulk_update_rejects_unknown_columns() {
    let pool = seeded_pool().await;
    let objs = vec![Widget {
        id: 1,
        name: "x".into(),
        qty: 1,
        note: "x".into(),
    }];

    let err = Widget::bulk_update(&objs, &["nope"], &pool)
        .await
        .expect_err("unknown column must error");
    match err {
        ExecError::Query(QueryError::UnknownField { field, .. }) => {
            assert_eq!(field, "nope");
        }
        other => panic!("expected UnknownField, got {other:?}"),
    }
}

#[tokio::test]
async fn bulk_update_empty_inputs_are_a_noop() {
    let pool = seeded_pool().await;
    let objs = vec![Widget {
        id: 1,
        name: "x".into(),
        qty: 1,
        note: "x".into(),
    }];

    assert_eq!(
        Widget::bulk_update(&[], &["name"], &pool).await.unwrap(),
        0,
        "empty objs → 0"
    );
    assert_eq!(
        Widget::bulk_update(&objs, &[], &pool).await.unwrap(),
        0,
        "empty fields → 0"
    );
    // Nothing was written.
    assert_eq!(row(&pool, 1).await, ("alpha".into(), 10, "original".into()));
}
