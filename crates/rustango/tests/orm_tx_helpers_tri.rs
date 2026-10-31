//! The `_tx` helpers for count, bulk insert, soft delete and get-or-create
//! run on the caller's transaction, so a rollback undoes them.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{BulkInsertQuery, CountQuery, Filter, Model as _, Op, SqlValue, WhereExpr};
use rustango::soft_delete::{soft_delete_tx, SoftDeleteError};
use rustango::sql::{
    bulk_insert_tx, count_tx, get_or_create_tx, insert_tx, transaction_pool, Auto,
    CounterPool as _, FetcherPool as _, Pool,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_tx_helpers_note", app = "orm_tx_helpers")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 32, unique)]
    pub slug: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Audited: its soft delete cannot be written inside a caller's tx.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "orm_tx_helpers_doc",
    app = "orm_tx_helpers",
    audit(track = "deleted_at")
)]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn rows(ids: std::ops::Range<i64>) -> BulkInsertQuery {
    BulkInsertQuery::new(
        Note::SCHEMA,
        vec!["id", "slug"],
        ids.map(|i| vec![SqlValue::I64(i), SqlValue::String(format!("n{i}"))])
            .collect(),
    )
}

fn all() -> CountQuery {
    CountQuery::new(Note::SCHEMA, WhereExpr::And(Vec::new()))
}

/// Rows the tx wrote are counted inside it and gone after a rollback.
async fn bulk_insert_and_count_join_the_tx(pool: &Pool) {
    let mut tx = transaction_pool(pool).await.unwrap();
    bulk_insert_tx(&mut tx, &rows(0..3)).await.unwrap();
    bulk_insert_tx(&mut tx, &rows(3..3)).await.unwrap();
    assert_eq!(count_tx(&mut tx, &all()).await.unwrap(), 3);
    tx.rollback().await.unwrap();
    assert_eq!(Note::objects().count(pool).await.unwrap(), 0);

    let mut tx = transaction_pool(pool).await.unwrap();
    bulk_insert_tx(&mut tx, &rows(0..2)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(Note::objects().count(pool).await.unwrap(), 2);
}

/// The soft delete is visible in the tx and undone by its rollback.
async fn soft_delete_joins_the_tx(pool: &Pool) {
    let mut tx = transaction_pool(pool).await.unwrap();
    bulk_insert_tx(&mut tx, &rows(0..2)).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = transaction_pool(pool).await.unwrap();
    let n = soft_delete_tx(&mut tx, Note::SCHEMA, "id", SqlValue::I64(0))
        .await
        .unwrap();
    assert_eq!(n, 1);
    let again = soft_delete_tx(&mut tx, Note::SCHEMA, "id", SqlValue::I64(0))
        .await
        .unwrap();
    assert_eq!(again, 0, "a deleted row is not deleted twice");
    let trashed = CountQuery::new(
        Note::SCHEMA,
        WhereExpr::Predicate(Filter::new("deleted_at", Op::IsNull, false)),
    );
    assert_eq!(count_tx(&mut tx, &trashed).await.unwrap(), 1);
    tx.rollback().await.unwrap();
    let live = Note::objects().fetch(pool).await.unwrap();
    assert!(live.iter().all(|n| n.deleted_at.is_none()), "{live:?}");
}

/// Refused before any write, so the audit trail cannot miss it.
async fn soft_delete_tx_refuses_an_audited_model(pool: &Pool) {
    let mut tx = transaction_pool(pool).await.unwrap();
    let err = soft_delete_tx(&mut tx, Doc::SCHEMA, "id", SqlValue::I64(1))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SoftDeleteError::Exec(rustango::sql::ExecError::AuditUnsupported { .. })
        ),
        "{err:?}"
    );
    let err = soft_delete_tx(&mut tx, Note::SCHEMA, "id", SqlValue::I64(1)).await;
    assert!(err.is_ok(), "an unaudited model is not refused: {err:?}");
}

/// Created once in the tx, then found; a rollback drops the row.
async fn get_or_create_joins_the_tx(pool: &Pool) {
    let mut tx = transaction_pool(pool).await.unwrap();
    for expect_created in [true, false] {
        let (note, created) = get_or_create_tx(
            Note::objects().filter("slug", "a"),
            |tx| {
                Box::pin(async move {
                    let q = rustango::core::InsertQuery::new(
                        Note::SCHEMA,
                        vec!["id", "slug"],
                        vec![SqlValue::I64(7), SqlValue::String("a".into())],
                    );
                    insert_tx(tx, &q).await?;
                    Ok(Note {
                        id: 7,
                        slug: "a".into(),
                        deleted_at: None,
                    })
                })
            },
            &mut tx,
        )
        .await
        .unwrap();
        assert_eq!((note.id, created), (7, expect_created));
    }
    tx.rollback().await.unwrap();
    assert_eq!(Note::objects().count(pool).await.unwrap(), 0);
}

/// Two matches are an error, not a pick.
async fn get_or_create_tx_refuses_two_matches(pool: &Pool) {
    let mut tx = transaction_pool(pool).await.unwrap();
    bulk_insert_tx(&mut tx, &rows(0..2)).await.unwrap();
    let res = get_or_create_tx(
        Note::objects().filter_op("id", Op::Gte, 0_i64),
        |_| Box::pin(async { Err(rustango::sql::sqlx::Error::RowNotFound.into()) }),
        &mut tx,
    )
    .await;
    assert!(
        matches!(
            res,
            Err(rustango::sql::ExecError::MultipleRowsReturned { count: 2, .. })
        ),
        "{res:?}"
    );
}

tri_dialect_test! {
    model: Note,
    scenarios: [
        bulk_insert_and_count_join_the_tx,
        soft_delete_joins_the_tx,
        soft_delete_tx_refuses_an_audited_model,
        get_or_create_joins_the_tx,
        get_or_create_tx_refuses_two_matches,
    ],
}
