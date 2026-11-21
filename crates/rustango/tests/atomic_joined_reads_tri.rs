//! Reads, raw SQL and DDL on `&pool` inside `atomic(&pool, …)` run on the
//! block's transaction. Most scenarios use one-connection pools, so a
//! statement that took a second connection would hang; `within` turns that
//! into a failure.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use std::future::Future;
use std::time::Duration;

use rustango::sql::{
    raw_execute_pool, raw_query_pool, run_ddl_idempotent, transaction_pool, AtomicTx,
    CounterPool as _, ExecError, ExistsPool as _, FetcherPool as _, Pool, SqlError,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "ajr_note")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub label: String,
}

/// A second, one-connection pool on the same database.
async fn one_conn(pool: &Pool) -> Pool {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => Pool::Postgres(
            rustango::sql::sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => Pool::Mysql(
            rustango::sql::sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => Pool::Sqlite(
            rustango::sql::sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
    }
}

fn note(id: i64, label: &str) -> Note {
    Note {
        id,
        label: label.into(),
    }
}

async fn put(tx: &AtomicTx, id: i64) -> Result<(), ExecError> {
    note(id, "n").insert_tx(&mut *tx.lock().await?).await
}

async fn seed(pool: &Pool, ids: &[i64], label: &str) {
    for &id in ids {
        note(id, label).insert(pool).await.unwrap();
    }
}

async fn ids(pool: &Pool) -> Vec<i64> {
    let rows: Vec<Note> = Note::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

fn bail() -> ExecError {
    ExecError::Sql(SqlError::EmptyInList)
}

fn dialect(pool: &Pool) -> rustango::sql::DialectKind {
    pool.dialect().kind()
}

/// Bound a scenario so a wait for a second connection fails instead of hanging.
async fn within<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("hung: a statement waited for a second connection")
}

/// Every read shape sees the block's uncommitted rows, on its connection.
async fn read_sees_uncommitted_write(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let seen = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        note(2, "n").insert(&q).await?;
        let rows: Vec<Note> = Note::objects().order_by(&[("id", false)]).fetch(&q).await?;
        let count = Note::objects().count(&q).await?;
        let exists = Note::objects().filter("id", 1_i64).exists(&q).await?;
        let first = Note::objects().first(&q).await?.map(|n| n.id);
        let values = Note::objects().values_list(&["id"]).fetch(&q).await?.len();
        let raw: Vec<(i64,)> =
            raw_query_pool("SELECT id FROM ajr_note ORDER BY id", vec![], &q).await?;
        Ok((
            rows.into_iter().map(|r| r.id).collect::<Vec<_>>(),
            count,
            exists,
            first,
            values,
            raw.len(),
        ))
    }))
    .await
    .unwrap();
    assert_eq!(seen, (vec![1, 2], 2, true, Some(1), 2, 2));
}

/// A failed read is one plain statement: on PG it aborts the block, which
/// then refuses every statement; MySQL and SQLite undo only the read. A
/// nested block around the read keeps the outer one going everywhere.
async fn failed_read_aborts_the_block_on_pg(pool: &Pool) {
    seed(pool, &[5], "n").await;
    let p = one_conn(pool).await;
    let q = p.clone();
    // Another transaction holds row 5, so a `nowait` read of it fails.
    let other = one_conn(pool).await;
    let mut held = transaction_pool(&other).await.unwrap();
    let locks = dialect(pool) != rustango::sql::DialectKind::Sqlite;
    if locks {
        Note::objects()
            .filter("id", 5_i64)
            .select_for_update()
            .fetch_on(&mut held)
            .await
            .unwrap();
    }
    let pg = dialect(pool) == rustango::sql::DialectKind::Postgres;
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        // Inside a nested block the failure stays there.
        let r = q.clone();
        let nested: Result<Vec<(i64,)>, ExecError> = rustango::atomic!(&r, |_sp| {
            raw_query_pool("SELECT id FROM ajr_missing", vec![], &r).await
        })
        .await;
        assert!(nested.is_err());
        put(tx, 2).await?;
        if locks {
            let busy = Note::objects()
                .filter("id", 5_i64)
                .select_for_update()
                .nowait()
                .fetch(&q)
                .await;
            assert!(matches!(busy, Err(ExecError::Driver(_))), "got {busy:?}");
        }
        let missing: Result<Vec<(i64,)>, _> =
            raw_query_pool("SELECT id FROM ajr_missing", vec![], &q).await;
        assert!(missing.is_err());
        let next = put(tx, 3).await;
        if pg {
            assert!(
                matches!(next, Err(ExecError::AtomicAborted)),
                "got {next:?}"
            );
            return Ok(());
        }
        next?;
        note(4, "n").insert(&q).await?;
        Ok(())
    }))
    .await;
    held.rollback().await.unwrap();
    if pg {
        assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
        assert_eq!(ids(pool).await, vec![5], "nothing committed");
    } else {
        res.expect("MySQL and SQLite undo only the failed read");
        assert_eq!(ids(pool).await, vec![1, 2, 3, 4, 5]);
    }
}

/// Joined writes dropped at any point, savepoint and RELEASE included,
/// leave the block usable.
async fn cancelled_joined_writes_leave_the_block_usable(pool: &Pool) {
    if dialect(pool) == rustango::sql::DialectKind::Mysql {
        return; // sqlx-mysql cannot recover a connection whose statement was dropped mid-reply.
    }
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        for i in 0..150_i64 {
            let row = note(100 + i, "c");
            let wait = Duration::from_micros(15 * u64::try_from(i).unwrap());
            let _ = tokio::time::timeout(wait, row.insert(&q)).await;
        }
        put(tx, 1).await?;
        Note::objects().count(&q).await?;
        Ok(())
    }))
    .await
    .expect("the block stays usable");
    assert!(ids(pool).await.contains(&1));
}

/// Two reads at once on one block: one runs, the other is refused, never both at once.
async fn concurrent_reads_are_refused(pool: &Pool) {
    let p = one_conn(pool).await;
    let (a, b) = (p.clone(), p.clone());
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        // In-process SQLite can finish the first read before the second starts, so the
        // reads may never overlap there; the held-guard test below covers its refusal.
        let mut seen = dialect(&p) == rustango::sql::DialectKind::Sqlite;
        for _ in 0..50 {
            let (ra, rb) = tokio::join!(Note::objects().fetch(&a), Note::objects().count(&b));
            let refused = [ra.as_ref().err(), rb.as_ref().err()]
                .into_iter()
                .filter(|e| matches!(e, Some(ExecError::NestedAtomic)))
                .count();
            assert!(ra.is_ok() || rb.is_ok(), "{ra:?} {rb:?}");
            assert!(
                refused + usize::from(ra.is_ok()) + usize::from(rb.is_ok()) == 2,
                "{ra:?} {rb:?}"
            );
            if refused == 1 {
                seen = true;
                break;
            }
        }
        assert!(seen, "two reads at once were never refused");
        Ok(())
    }))
    .await
    .unwrap();
}

/// A read while the block's guard is held is refused, not run beside it.
async fn read_with_guard_held_is_refused(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        let guard = tx.lock().await?;
        let r = Note::objects().fetch(&q).await;
        assert!(matches!(r, Err(ExecError::NestedAtomic)), "got {r:?}");
        drop(guard);
        Note::objects().fetch(&q).await?;
        Ok(())
    }))
    .await
    .unwrap();
}

/// In-memory SQLite: a read after a write in the block used to wait on the
/// block's table lock from a second connection, forever.
async fn in_memory_sqlite_read_after_write_does_not_hang(pool: &Pool) {
    if dialect(pool) != rustango::sql::DialectKind::Sqlite {
        return;
    }
    let q = pool.clone();
    let n = within(rustango::atomic!(pool, |_tx| {
        note(1, "n").insert(&q).await?;
        Note::objects().count(&q).await
    }))
    .await
    .unwrap();
    assert_eq!(n, 1);
}

/// A raw `COMMIT` is found at the outermost commit; the statements after it
/// ran in autocommit.
async fn raw_commit_ends_the_block_early(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        raw_execute_pool(&q, "COMMIT", vec![]).await?;
        put(tx, 2).await?;
        Ok(())
    }))
    .await;
    assert!(
        matches!(res, Err(ExecError::AtomicEndedEarly)),
        "got {res:?}"
    );
    assert_eq!(ids(pool).await, vec![1, 2], "both rows are committed");
}

/// MySQL commits on DDL, so DDL through the pool is refused in a block. On
/// PG and SQLite it joins the block and rolls back with it.
async fn mysql_ddl_via_pool_is_refused(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, "ajr_ddl_probe").await;
    let p = one_conn(pool).await;
    let q = p.clone();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let ddl = std::sync::Arc::clone(&seen);
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let r = run_ddl_idempotent(&q, "CREATE TABLE ajr_ddl_probe (id INT)").await;
        *ddl.lock().unwrap() = r.err().map(|e| e.to_string());
        // PG: a re-run's caught "already exists" must not abort the block.
        if q.dialect().kind() == rustango::sql::DialectKind::Postgres {
            run_ddl_idempotent(&q, "CREATE TABLE ajr_ddl_probe (id INT)")
                .await
                .expect("idempotent re-run");
        }
        put(tx, 2).await?;
        Err(bail())
    }))
    .await;
    let ddl_err = seen.lock().unwrap().take();
    let created: Result<Vec<(i64,)>, _> =
        raw_query_pool("SELECT id FROM ajr_ddl_probe", vec![], pool).await;
    rustango::testkit::matrix::drop_table(pool, "ajr_ddl_probe").await;
    assert!(
        matches!(res, Err(ExecError::Sql(SqlError::EmptyInList))),
        "the block failed early: {res:?}"
    );
    if dialect(pool) == rustango::sql::DialectKind::Mysql {
        let err = ddl_err.expect("MySQL DDL in a block is refused");
        assert!(err.contains("refused inside an atomic block"), "got {err}");
    } else {
        assert_eq!(ddl_err, None);
    }
    assert!(created.is_err(), "the table outlived the rolled-back block");
    assert_eq!(ids(pool).await, Vec::<i64>::new(), "nothing committed");
}

/// The schema helpers refuse DDL in a MySQL block too.
async fn mysql_schema_helpers_are_refused(pool: &Pool) {
    if dialect(pool) != rustango::sql::DialectKind::Mysql {
        return;
    }
    let p = one_conn(pool).await;
    let q = p.clone();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let err = std::sync::Arc::clone(&seen);
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let r = rustango::testkit::create_tables_for::<Note>(&q).await;
        *err.lock().unwrap() = r.err().map(|e| e.to_string());
        Ok(())
    }))
    .await;
    res.expect("the block commits: no implicit commit ended it");
    let err = seen.lock().unwrap().take().expect("refused");
    assert!(err.contains("refused inside an atomic block"), "got {err}");
    assert_eq!(ids(pool).await, vec![1]);
}

/// Statements no transaction can hold are refused by the server.
async fn non_transactional_statements_are_refused(pool: &Pool) {
    let sql = match dialect(pool) {
        rustango::sql::DialectKind::Postgres => {
            "CREATE INDEX CONCURRENTLY ajr_label_idx ON ajr_note (label)"
        }
        rustango::sql::DialectKind::Sqlite => "VACUUM",
        rustango::sql::DialectKind::Mysql => return, // MySQL: every DDL is refused, see above.
    };
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let r = raw_execute_pool(&q, sql, vec![]).await;
        assert!(matches!(r, Err(ExecError::Driver(_))), "got {r:?}");
        put(tx, 2).await?;
        Ok(())
    }))
    .await;
    // PG aborts the block on the failed statement; SQLite undoes only it.
    if dialect(pool) == rustango::sql::DialectKind::Postgres {
        assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
        assert_eq!(ids(pool).await, Vec::<i64>::new());
    } else {
        res.expect("the block survives the refused statement");
        assert_eq!(ids(pool).await, vec![1, 2]);
    }
}

/// Rows the loop moves out of the filter must not shift the next chunk.
async fn iterator_updating_rows_out_of_filter_visits_all(pool: &Pool) {
    seed(pool, &[1, 2, 3, 4, 5, 6], "todo").await;
    let q = pool.clone();
    let visited = within(rustango::atomic!(pool, |_tx| {
        let mut it = Note::objects().filter("label", "todo").iterator(2)?;
        let mut visited = Vec::new();
        while let Some(chunk) = it.next_chunk(&q).await? {
            for mut n in chunk {
                visited.push(n.id);
                n.label = "done".into();
                n.save(&q).await?;
            }
        }
        Ok(visited)
    }))
    .await
    .unwrap();
    assert_eq!(visited, vec![1, 2, 3, 4, 5, 6]);
}

/// Keyset paging needs the primary-key order.
async fn iterator_with_other_order_is_refused(pool: &Pool) {
    seed(pool, &[1, 2], "n").await;
    let q = pool.clone();
    within(rustango::atomic!(pool, |_tx| {
        let mut it = Note::objects().order_by(&[("label", false)]).iterator(1)?;
        let r = it.next_chunk(&q).await;
        assert!(matches!(r, Err(ExecError::InAtomic { .. })), "got {r:?}");
        Ok(())
    }))
    .await
    .unwrap();
    let mut it = Note::objects()
        .order_by(&[("label", false)])
        .iterator(1)
        .unwrap();
    assert!(it.next_chunk(pool).await.unwrap().is_some());
}

tri_dialect_test! {
    model: Note,
    scenarios: [
        read_sees_uncommitted_write,
        failed_read_aborts_the_block_on_pg,
        cancelled_joined_writes_leave_the_block_usable,
        concurrent_reads_are_refused,
        read_with_guard_held_is_refused,
        in_memory_sqlite_read_after_write_does_not_hang,
        raw_commit_ends_the_block_early,
        mysql_ddl_via_pool_is_refused,
        non_transactional_statements_are_refused,
        mysql_schema_helpers_are_refused,
        iterator_updating_rows_out_of_filter_visits_all,
        iterator_with_other_order_is_refused,
    ],
}

/// SQLite takes the write lock at the block's `BEGIN`: another writer
/// cannot slip in before the block's first write.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_atomic_begins_immediate() {
    use rustango::sql::sqlx::sqlite::SqlitePoolOptions;

    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let pool = rustango::testkit::matrix::sqlite_file_pool().await;
    rustango::testkit::matrix::fresh_table::<Note>(&pool).await;
    let opts = (*pool.as_sqlite().unwrap().connect_options())
        .clone()
        .busy_timeout(Duration::from_millis(200));
    let other = Pool::Sqlite(
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap(),
    );
    within(rustango::atomic!(&pool, |_tx| {
        let r = note(9, "theirs").insert(&other).await;
        assert!(r.is_err(), "another writer got in before the block wrote");
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(ids(&pool).await, Vec::<i64>::new());
}
