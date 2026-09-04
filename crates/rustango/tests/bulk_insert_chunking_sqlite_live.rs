//! `bulk_insert_pool` must batch against the backend's bind-parameter
//! ceiling (#1284).
//!
//! One multi-row INSERT binds `rows × columns` parameters and every
//! backend caps that: 65535 on Postgres (an int16 on the wire), 32766 on
//! modern SQLite, `max_allowed_packet` on MySQL. Before this fix
//! `bulk_insert_pool` emitted a single statement no matter the size, so
//! a large import failed with an opaque driver error. Django's
//! `bulk_create` batches for exactly this reason.
//!
//! SQLite is the right place to test it: its 32766 ceiling is the
//! lowest of the three, so the row count stays small enough to run in
//! milliseconds.
//!
//! ```bash
//! cargo test -p rustango --no-default-features --features sqlite \
//!   --test bulk_insert_chunking_sqlite_live
//! ```

#![cfg(feature = "sqlite")]

use rustango::core::{BulkInsertQuery, Model as _, SqlValue};
use rustango::sql::{bulk_insert_pool, raw_execute_pool, raw_query_pool, sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bulk_chunk")]
#[rustango(app = "bulk_chunk_app")]
pub struct Chunked {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub a: i64,
    #[rustango(max_length = 50)]
    pub b: String,
    pub c: i64,
    #[rustango(max_length = 50)]
    pub d: String,
}

/// Four bound columns per row (the `Auto` pk is left to the database),
/// so SQLite's 32766 ceiling lands the split at 8191 rows.
const COLUMNS: usize = 4;
const ROWS_PER_BATCH: usize = 32766 / COLUMNS; // 8191
const ROWS: usize = 10_000;

async fn pool_with_table() -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite pool"),
    );
    raw_execute_pool(
        &pool,
        "CREATE TABLE bulk_chunk (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            a INTEGER NOT NULL, b TEXT NOT NULL, \
            c INTEGER NOT NULL, d TEXT NOT NULL)",
        vec![],
    )
    .await
    .expect("create table");
    pool
}

fn query(rows: Vec<Vec<SqlValue>>) -> BulkInsertQuery {
    BulkInsertQuery {
        model: Chunked::SCHEMA,
        columns: vec!["a", "b", "c", "d"],
        rows,
        returning: Vec::new(),
        on_conflict: None,
    }
}

#[tokio::test]
async fn bulk_insert_batches_past_the_bind_parameter_ceiling() {
    let pool = pool_with_table().await;

    let rows: Vec<Vec<SqlValue>> = (0..ROWS)
        .map(|i| {
            vec![
                SqlValue::I64(i as i64),
                SqlValue::String(format!("row-{i}")),
                SqlValue::I64((i * 2) as i64),
                SqlValue::String("filler".to_owned()),
            ]
        })
        .collect();
    assert_eq!(rows[0].len(), COLUMNS);
    assert!(
        ROWS > ROWS_PER_BATCH,
        "the fixture must exceed one batch or it proves nothing"
    );

    // Before the fix this returned a driver error ("too many SQL
    // variables") rather than inserting anything.
    bulk_insert_pool(&pool, &query(rows))
        .await
        .expect("bulk_insert must batch, not overflow the parameter limit");

    let counted: Vec<(i64,)> = raw_query_pool("SELECT COUNT(*) FROM bulk_chunk", vec![], &pool)
        .await
        .expect("count");
    assert_eq!(
        counted[0].0, ROWS as i64,
        "every row must land, across all batches"
    );

    // Batching must not scramble or drop values at a chunk boundary.
    let edge: Vec<(i64, String)> = raw_query_pool(
        "SELECT a, b FROM bulk_chunk WHERE a IN (0, 8190, 8191, 8192, 9999) ORDER BY a",
        vec![],
        &pool,
    )
    .await
    .expect("edge rows");
    let got: Vec<(i64, &str)> = edge.iter().map(|(a, b)| (*a, b.as_str())).collect();
    assert_eq!(
        got,
        vec![
            (0, "row-0"),
            (8190, "row-8190"),
            (8191, "row-8191"),
            (8192, "row-8192"),
            (9999, "row-9999"),
        ],
        "values must stay aligned with their row across the chunk split"
    );
}

/// A batch that fits must still be a single statement — the fast path
/// is unchanged for the overwhelmingly common small insert.
#[tokio::test]
async fn small_bulk_insert_still_round_trips() {
    let pool = pool_with_table().await;
    let rows = vec![
        vec![
            SqlValue::I64(1),
            SqlValue::String("one".into()),
            SqlValue::I64(10),
            SqlValue::String("x".into()),
        ],
        vec![
            SqlValue::I64(2),
            SqlValue::String("two".into()),
            SqlValue::I64(20),
            SqlValue::String("y".into()),
        ],
    ];
    bulk_insert_pool(&pool, &query(rows))
        .await
        .expect("small insert");

    let counted: Vec<(i64,)> = raw_query_pool("SELECT COUNT(*) FROM bulk_chunk", vec![], &pool)
        .await
        .expect("count");
    assert_eq!(counted[0].0, 2);
}
