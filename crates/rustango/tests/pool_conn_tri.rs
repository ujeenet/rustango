//! `pool.acquire()` holds one connection and every builder's `_on`
//! method runs on it, on each backend (#2675).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::AggregateExpr;
use rustango::query::QuerySet;
use rustango::sql::sqlx;
use rustango::sql::{CounterPool as _, FetcherPool as _, ForeignKey, Pool, PoolConn};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone, PartialEq)]
#[rustango(table = "pc_author")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pc_book")]
pub struct Book {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub title: String,
    pub author: ForeignKey<Author>,
}

async fn seeded(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "pc_book").await;
    fresh_table::<Author>(pool).await;
    for (id, name) in [(1, "Ada"), (2, "Bob")] {
        Author {
            id,
            name: name.into(),
        }
        .insert_pool(pool)
        .await
        .expect("seed author");
    }
    fresh_table::<Book>(pool).await;
    for (id, author) in [(1, 1_i64), (2, 1), (3, 2)] {
        Book {
            id,
            title: format!("b{id}"),
            author: ForeignKey::unloaded(author),
        }
        .insert_pool(pool)
        .await
        .expect("seed book");
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

/// The `_on` methods that took only a Postgres executor run on a held
/// connection on every backend (#1395).
async fn former_pg_only_on_methods_run_on_a_held_conn(pool: &Pool) {
    use rustango::sql::{annotate_count_children_on, fetch_aggregate_on, ExplainOptions};
    let mut conn = pool.acquire().await.expect("acquire");
    let c = &mut conn;

    let page = by_id().limit(1).fetch_paginated_on(&mut *c).await.unwrap();
    assert_eq!((page.rows.len(), page.total), (1, 2));

    let bulk = Author::objects()
        .in_bulk_on(Author::id, [1_i64, 2], |a| a.id, &mut *c)
        .await
        .unwrap();
    assert_eq!(bulk[&2].name, "Bob");

    let counts = annotate_count_children_on::<Author, _>(by_id(), "pc_book", "author", &mut *c)
        .await
        .unwrap();
    let counts: Vec<(i64, i64)> = counts.into_iter().map(|(a, n)| (a.id, n)).collect();
    assert_eq!(counts, [(1, 2), (2, 1)]);

    let plan = by_id().explain_on(&mut *c, ExplainOptions::default()).await.unwrap();
    assert!(!plan.is_empty());

    let agg = Author::objects()
        .aggregate()
        .values(&[])
        .annotate("n", AggregateExpr::Count(None))
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&agg, &mut *c).await.unwrap();
    assert_eq!(rows.len(), 1);

    let mut book = Book::objects().filter("id", 3_i64).fetch_on(&mut *c).await.unwrap().remove(0);
    assert_eq!(book.author.get_on(&mut *c).await.unwrap().name, "Bob");

    let renamed = Author::objects()
        .filter("id", 2_i64)
        .update()
        .set("name", "Cy")
        .execute_on(&mut *c)
        .await
        .unwrap();
    assert_eq!(renamed, 1);
    let gone = Book::objects().filter("id", 3_i64).delete_on(&mut *c).await.unwrap();
    assert_eq!(gone, 1);
    drop(conn);
    assert_eq!(Book::objects().count(pool).await.unwrap(), 2);
    let names: Vec<String> = by_id().fetch(pool).await.unwrap().into_iter().map(|a| a.name).collect();
    assert_eq!(names, ["Ada", "Cy"]);
}

/// A one-connection pool on the same database, so a release is visible.
async fn one_conn(pool: &Pool) -> Pool {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => Pool::Postgres(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => Pool::Mysql(
            sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => Pool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
    }
}

/// Read one session setting on `conn`, after setting it when `set`.
async fn session_setting(conn: &mut PoolConn, set: bool) -> String {
    match conn {
        #[cfg(feature = "postgres")]
        PoolConn::Postgres(c) => {
            if set {
                sqlx::query("SET application_name = 'rustango_leak'").execute(&mut **c).await.unwrap();
            }
            sqlx::query_scalar("SELECT current_setting('application_name')")
                .fetch_one(&mut **c)
                .await
                .unwrap()
        }
        #[cfg(feature = "mysql")]
        PoolConn::Mysql(c) => {
            if set {
                sqlx::query("SET @rustango_leak = 'rustango_leak'").execute(&mut **c).await.unwrap();
            }
            sqlx::query_scalar("SELECT CAST(COALESCE(@rustango_leak, '') AS CHAR)")
                .fetch_one(&mut **c)
                .await
                .unwrap()
        }
        #[cfg(feature = "sqlite")]
        PoolConn::Sqlite(c) => {
            if set {
                sqlx::query("PRAGMA cache_size = 1234").execute(&mut **c).await.unwrap();
            }
            sqlx::query_scalar::<_, i64>("PRAGMA cache_size")
                .fetch_one(&mut **c)
                .await
                .unwrap()
                .to_string()
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("no backend enabled"),
    }
}

/// Session state set on a held connection reaches the next borrower,
/// unless the connection is closed on drop (#1397).
async fn close_on_drop_keeps_session_state_out_of_the_pool(pool: &Pool) {
    let p = one_conn(pool).await;
    let mut conn = p.acquire().await.unwrap();
    let default = session_setting(&mut conn, false).await;
    let set = session_setting(&mut conn, true).await;
    assert_ne!(set, default);
    drop(conn);
    let mut conn = p.acquire().await.unwrap();
    assert_eq!(session_setting(&mut conn, false).await, set, "premise: sqlx keeps it");
    conn.close_on_drop();
    drop(conn);
    let mut conn = p.acquire().await.unwrap();
    assert_eq!(session_setting(&mut conn, false).await, default);
}

tri_dialect_test!(
    setup: seeded,
    scenarios: [
        held_conn_is_one_session,
        on_conn_matches_pool,
        queries_run_on_the_held_session,
        on_tx_sees_its_writes,
        former_pg_only_on_methods_run_on_a_held_conn,
        close_on_drop_keeps_session_state_out_of_the_pool,
    ]
);
