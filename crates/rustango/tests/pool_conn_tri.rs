//! `pool.acquire()` holds one connection and every builder's `_on`
//! method runs on it, on each backend (#2675).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::AggregateExpr;
use rustango::query::QuerySet;
use rustango::sql::sqlx;
use rustango::sql::{CounterPool as _, FetcherPool as _, Pool, PoolConn};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone, PartialEq)]
#[rustango(table = "pc_author")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Author>(pool).await;
    for (id, name) in [(1, "Ada"), (2, "Bob")] {
        Author {
            id,
            name: name.into(),
        }
        .insert_pool(pool)
        .await
        .expect("seed author");
    }
}

/// The backend's id for the session `conn` holds.
async fn session_id(conn: &mut PoolConn) -> String {
    match conn {
        #[cfg(feature = "postgres")]
        PoolConn::Postgres(c) => sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
            .fetch_one(&mut **c)
            .await
            .expect("pid")
            .to_string(),
        #[cfg(feature = "mysql")]
        PoolConn::Mysql(c) => sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()")
            .fetch_one(&mut **c)
            .await
            .expect("connection id")
            .to_string(),
        #[cfg(feature = "sqlite")]
        PoolConn::Sqlite(c) => {
            let mut h = c.lock_handle().await.expect("handle");
            format!("{:p}", h.as_raw_handle())
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("no backend enabled"),
    }
}

/// Premise: a held connection is one backend session; another is not.
async fn held_conn_is_one_session(pool: &Pool) {
    let mut a = pool.acquire().await.expect("acquire a");
    let mut b = pool.acquire().await.expect("acquire b");
    let first = session_id(&mut a).await;
    assert_eq!(first, session_id(&mut a).await);
    assert_ne!(first, session_id(&mut b).await);
}

fn by_id() -> QuerySet<Author> {
    Author::objects().order_by(&[("id", false)])
}

/// Every builder's `_on` returns what its pool twin returns.
async fn on_conn_matches_pool(pool: &Pool) {
    let mut conn = pool.acquire().await.expect("acquire");
    let c = &mut conn;
    assert_eq!(by_id().fetch_on(&mut *c).await.unwrap(), by_id().fetch(pool).await.unwrap());
    assert_eq!(by_id().fetch_on(pool).await.unwrap().len(), 2);
    assert_eq!(by_id().count_on(&mut *c).await.unwrap(), 2);
    assert!(by_id().exists_on(&mut *c).await.unwrap());
    assert!(!by_id().filter("id", 9_i64).exists_on(&mut *c).await.unwrap());
    assert_eq!(by_id().first_on(&mut *c).await.unwrap().map(|a| a.name), Some("Ada".into()));
    assert_eq!(
        by_id().values_dict(&["name"]).fetch_on(&mut *c).await.unwrap(),
        by_id().values_dict(&["name"]).fetch(pool).await.unwrap()
    );
    assert_eq!(
        by_id().values_list(&["id", "name"]).fetch_on(&mut *c).await.unwrap(),
        by_id().values_list(&["id", "name"]).fetch(pool).await.unwrap()
    );
    assert_eq!(
        by_id().values_list_flat("name").fetch_on::<String>(&mut *c).await.unwrap(),
        ["Ada", "Bob"]
    );
    assert_eq!(
        by_id().values_list_flat("name").first_on::<String>(&mut *c).await.unwrap(),
        Some("Ada".into())
    );
    let agg = || Author::objects().aggregate().annotate("n", AggregateExpr::Count(None));
    assert_eq!(
        agg().fetch_on(&mut *c).await.unwrap(),
        agg().fetch(pool).await.unwrap()
    );
}

/// The `_on` queries run on the held session: a TEMP table there hides
/// the real one, which the pool still sees.
async fn queries_run_on_the_held_session(pool: &Pool) {
    let mut conn = pool.acquire().await.expect("acquire");
    let ddl = "CREATE TEMPORARY TABLE pc_author (id BIGINT PRIMARY KEY, name VARCHAR(40) NOT NULL)";
    match &mut conn {
        #[cfg(feature = "postgres")]
        PoolConn::Postgres(c) => drop(sqlx::query(ddl).execute(&mut **c).await.unwrap()),
        #[cfg(feature = "mysql")]
        PoolConn::Mysql(c) => drop(sqlx::query(ddl).execute(&mut **c).await.unwrap()),
        #[cfg(feature = "sqlite")]
        PoolConn::Sqlite(c) => drop(sqlx::query(ddl).execute(&mut **c).await.unwrap()),
        #[allow(unreachable_patterns)]
        _ => unreachable!("no backend enabled"),
    }
    let c = &mut conn;
    assert!(by_id().fetch_on(&mut *c).await.unwrap().is_empty());
    assert_eq!(by_id().count_on(&mut *c).await.unwrap(), 0);
    assert!(!by_id().exists_on(&mut *c).await.unwrap());
    assert_eq!(by_id().first_on(&mut *c).await.unwrap(), None);
    assert!(by_id().values_dict(&["name"]).fetch_on(&mut *c).await.unwrap().is_empty());
    assert!(by_id().values_list(&["name"]).fetch_on(&mut *c).await.unwrap().is_empty());
    assert!(by_id()
        .values_list_flat("name")
        .fetch_on::<String>(&mut *c)
        .await
        .unwrap()
        .is_empty());
    let agg = || Author::objects().aggregate().annotate("n", AggregateExpr::Count(None));
    assert!(agg().fetch_on(&mut *c).await.unwrap().is_empty());
    assert!(!agg().fetch(pool).await.unwrap().is_empty());
    assert_eq!(by_id().count(pool).await.unwrap(), 2);
}

/// A transaction opened on the held connection reads its own writes
/// through `_on`; so does one from `transaction_pool`.
async fn on_tx_sees_its_writes(pool: &Pool) {
    let mut conn = pool.acquire().await.expect("acquire");
    let mut tx = conn.begin().await.expect("begin");
    let cy = Author {
        id: 3,
        name: "Cy".into(),
    };
    cy.insert_tx(&mut tx).await.expect("insert");
    assert_eq!(by_id().count_on(&mut tx).await.unwrap(), 3);
    assert_eq!(by_id().fetch_on(&mut tx).await.unwrap().len(), 3);
    assert_eq!(
        by_id().values_list_flat("name").fetch_on::<String>(&mut tx).await.unwrap(),
        ["Ada", "Bob", "Cy"]
    );
    tx.rollback().await.expect("rollback");
    assert_eq!(by_id().count_on(&mut conn).await.unwrap(), 2);
    drop(conn);

    let mut tx = rustango::sql::transaction_pool(pool).await.expect("begin");
    cy.insert_tx(&mut tx).await.expect("insert");
    assert!(by_id().filter("id", 3_i64).exists_on(&mut tx).await.unwrap());
    tx.rollback().await.expect("rollback");
}

tri_dialect_test!(
    setup: seeded,
    scenarios: [
        held_conn_is_one_session,
        on_conn_matches_pool,
        queries_run_on_the_held_session,
        on_tx_sees_its_writes,
    ]
);
