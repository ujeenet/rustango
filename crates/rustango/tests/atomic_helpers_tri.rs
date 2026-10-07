//! Multi-statement writes inside an `atomic` block on the same pool run in
//! a savepoint of it, not a second transaction (#1460). One-connection
//! pools: a second transaction would wait forever, and `within` turns
//! that into a failure.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use std::future::Future;
use std::time::Duration;

use rustango::audit::{self, AuditLog};
use rustango::sql::{Auto, CounterPool as _, ExecError, FetcherPool as _, Pool, SqlError};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "ahx_doc", app = "ahx", audit(track = "title"))]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ahx_post",
    app = "ahx",
    m2m(
        name = "tags",
        to = "ahx_tag",
        through = "ahx_post_tag",
        src = "post_id",
        dst = "tag_id"
    )
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ahx_post_tag", app = "ahx")]
pub struct PostTag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub post_id: i64,
    pub tag_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ahx_plain", app = "ahx")]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub label: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
    rustango::testkit::matrix::fresh_table::<PostTag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Plain>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    AuditLog::delete_where("entity_table", "ahx_doc", pool)
        .await
        .expect("clear audit rows");
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

/// Bound a scenario so a deadlock fails instead of hanging.
async fn within<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("deadlock: a helper waited for a second connection")
}

fn bail() -> ExecError {
    ExecError::Sql(SqlError::EmptyInList)
}

async fn audit_rows(pool: &Pool) -> i64 {
    AuditLog::objects()
        .filter("entity_table", "ahx_doc")
        .count(pool)
        .await
        .expect("count audit rows")
}

/// Audited writes, one per audit helper the derive routes through.
async fn audited_writes(q: &Pool) -> Result<(), ExecError> {
    let mut doc = Doc {
        id: Auto::default(),
        title: "a".into(),
    };
    doc.insert_pool(q).await?;
    doc.title = "b".into();
    doc.save_pool(q).await?;
    Doc::update_all("title", "c", q).await?;
    let mut gone = Doc {
        id: Auto::default(),
        title: "x".into(),
    };
    gone.insert_pool(q).await?;
    gone.delete_pool(q).await?;
    Ok(())
}

async fn audited_writes_join_the_block(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |_tx| {
        audited_writes(&q).await?;
        Err(bail())
    }))
    .await;
    assert!(res.is_err());
    assert_eq!(Doc::objects().count(pool).await.unwrap(), 0);
    assert_eq!(audit_rows(pool).await, 0, "audit rows outlived the block");

    let q = p.clone();
    within(rustango::atomic!(&p, |_tx| { audited_writes(&q).await }))
        .await
        .expect("commits");
    assert_eq!(Doc::objects().count(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 5);
}

async fn m2m_set_joins_the_block(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |_tx| {
        Post { id: 1 }.tags_m2m().set(&[1, 2], &q).await?;
        Err(bail())
    }))
    .await;
    assert!(res.is_err());
    assert_eq!(PostTag::objects().count(pool).await.unwrap(), 0);

    let q = p.clone();
    within(rustango::atomic!(&p, |_tx| {
        Post { id: 1 }.tags_m2m().set(&[1, 2], &q).await
    }))
    .await
    .expect("commits");
    assert_eq!(PostTag::objects().count(pool).await.unwrap(), 2);
}

async fn fixtures_join_the_block(pool: &Pool) {
    let fixture = rustango::fixtures::Fixture::new("plain")
        .from_value(serde_json::json!([{ "id": 1, "label": "one" }]))
        .expect("fixture");
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |_tx| {
        rustango::fixtures::load_all_pool(&[("ahx_plain", &fixture)], &q)
            .await
            .expect("load");
        Err(bail())
    }))
    .await;
    assert!(res.is_err());
    assert_eq!(Plain::objects().count(pool).await.unwrap(), 0);
}

/// Only MySQL's `incr` runs two statements; elsewhere it is one upsert.
async fn cache_incr_joins_the_block(pool: &Pool) {
    #[cfg(not(feature = "cache"))]
    let _ = pool;
    #[cfg(feature = "cache")]
    if pool.dialect().name() == "mysql" {
        use rustango::cache::{Cache as _, DatabaseCache};
        let p = one_conn(pool).await;
        let cache = DatabaseCache::new(p.clone(), "ahx_cache");
        cache.drop_table().await.expect("drop");
        cache.ensure_table().await.expect("table");
        let c = DatabaseCache::new(p.clone(), "ahx_cache");
        let res: Result<(), ExecError> = within(rustango::atomic!(&p, |_tx| {
            c.incr("hits", 2, None).await.expect("incr");
            Err(bail())
        }))
        .await;
        assert!(res.is_err());
        assert_eq!(cache.get("hits").await.expect("get"), None);
    }
}

/// Holding the block's guard across a joining write is refused, not a hang.
async fn held_guard_is_refused(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        let guard = tx.lock().await?;
        let mut doc = Doc {
            id: Auto::default(),
            title: "a".into(),
        };
        let res = doc.insert_pool(&q).await;
        assert!(matches!(res, Err(ExecError::NestedAtomic)), "{res:?}");
        drop(guard);
        doc.insert_pool(&q).await
    }))
    .await
    .expect("commits after the guard is dropped");
    assert_eq!(Doc::objects().count(pool).await.unwrap(), 1);
}

/// A scope dropped mid-savepoint is rolled back; the block stays usable.
async fn dropped_scope_rolls_back(pool: &Pool) {
    use rustango::__private_runtime::{Begin, TxScope};
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        let mut scope = TxScope::begin(&q, Begin::Deferred).await?;
        let gone = Plain {
            id: 1,
            label: "gone".into(),
        };
        gone.insert_tx(scope.tx()).await?;
        drop(scope);
        let kept = Plain {
            id: 2,
            label: "kept".into(),
        };
        kept.insert_tx(&mut *tx.lock().await?).await
    }))
    .await
    .expect("commits");
    let ids: Vec<i64> = Plain::objects()
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![2]);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        held_guard_is_refused,
        dropped_scope_rolls_back,
        audited_writes_join_the_block,
        m2m_set_joins_the_block,
        fixtures_join_the_block,
        cache_incr_joins_the_block,
    ],
}
