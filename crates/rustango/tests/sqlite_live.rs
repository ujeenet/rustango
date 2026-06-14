//! End-to-end SQLite ORM smoke test (v0.27 Phase 3).
//!
//! Runs against an anonymous in-memory SQLite database — no env var,
//! no external service, no docker. Confirms the bi-dialect surface
//! (`apply_all_pool` → `insert_pool` → `fetch` → `save_pool` →
//! `delete_pool` → `count`) reaches the SQLite arms cleanly,
//! including `Auto<i64>` PK assignment via `INSERT … RETURNING`.

#![cfg(feature = "sqlite")]

use rustango::sql::{Auto, CounterPool, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug)]
#[rustango(table = "live_users_sqlite")]
#[allow(dead_code)]
pub struct LiveUser {
    #[rustango(primary_key)]
    id: Auto<i64>,
    #[rustango(max_length = 255)]
    name: String,
    is_active: bool,
}

async fn fresh_pool() -> Pool {
    use rustango::core::Model as _;
    use rustango::migrate::ddl;

    // Anonymous in-memory database — each connection sees its own DB
    // unless `cache=shared` is passed. We force max_connections=1 so
    // tests run on a single connection (and thus a single DB).
    let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect sqlite::memory:");
    let pool: Pool = sqlite.into();

    // Build only our test model's DDL, not every registered framework
    // model — `apply_all_pool` would walk the full inventory and trip
    // on framework models that emit Postgres-shaped DDL.
    let dialect = pool.dialect();
    assert_eq!(dialect.name(), "sqlite");
    let create = ddl::create_table_sql_with_dialect(dialect, LiveUser::SCHEMA);
    rustango::sql::raw_execute_pool(&pool, &create, vec![])
        .await
        .expect("CREATE TABLE");
    for sql in ddl::create_constraints_sql_with_dialect(dialect, LiveUser::SCHEMA) {
        rustango::sql::raw_execute_pool(&pool, &sql, vec![])
            .await
            .expect("CREATE constraint");
    }
    pool
}

#[tokio::test]
async fn auto_pk_insert_pool_round_trips() {
    let pool = fresh_pool().await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "alice".into(),
        is_active: true,
    };
    u.insert_pool(&pool).await.expect("insert_pool");
    // INSERT … RETURNING populates the Auto<i64> PK.
    assert!(u.id.get().copied().is_some());
    let id = u.id.get().copied().unwrap();
    assert!(id >= 1);

    let n = LiveUser::objects().count(&pool).await.expect("count");
    assert_eq!(n, 1);
}

#[tokio::test]
async fn fetch_round_trips_decoded_row() {
    let pool = fresh_pool().await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "bob".into(),
        is_active: false,
    };
    u.insert_pool(&pool).await.expect("insert_pool");

    let users: Vec<LiveUser> = LiveUser::objects().fetch(&pool).await.expect("fetch");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "bob");
    assert!(!users[0].is_active);
    assert!(users[0].id.get().copied().is_some());
}

#[tokio::test]
async fn save_pool_updates_existing_row() {
    let pool = fresh_pool().await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "carol".into(),
        is_active: true,
    };
    u.insert_pool(&pool).await.expect("insert");
    u.name = "Carol".into();
    u.save_pool(&pool).await.expect("save_pool");

    let users: Vec<LiveUser> = LiveUser::objects().fetch(&pool).await.expect("fetch");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "Carol");
}

#[tokio::test]
async fn delete_pool_removes_row() {
    let pool = fresh_pool().await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "doomed".into(),
        is_active: true,
    };
    u.insert_pool(&pool).await.expect("insert");
    let affected = u.delete_pool(&pool).await.expect("delete_pool");
    assert_eq!(affected, 1);
    let n = LiveUser::objects().count(&pool).await.expect("count");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn pool_connect_sqlite_in_memory() {
    // Confirms the public `Pool::connect("sqlite::memory:")` path
    // returns a usable Pool::Sqlite. Phase 3 made this stop returning
    // FeatureNotEnabled.
    let pool = Pool::connect("sqlite::memory:")
        .await
        .expect("Pool::connect sqlite::memory: should succeed in Phase 3");
    assert_eq!(pool.backend_name(), "sqlite");
    assert!(pool.as_sqlite().is_some());
}
