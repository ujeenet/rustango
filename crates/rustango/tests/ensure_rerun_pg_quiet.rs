//! A re-run of the idempotent bootstrap sends Postgres nothing that fails, so
//! the server logs no ERROR per existing FK (#1406).
//!
//! Every failed statement aborts its transaction, which `pg_stat_database`
//! counts in `xact_rollback`; one deliberate failure marks the stats flushed.

#![cfg(all(feature = "postgres", feature = "testkit"))]

use rustango::sql::{raw_execute_pool, raw_query_pool, Pool};

#[derive(rustango::Model)]
#[rustango(table = "erq_parent")]
#[allow(dead_code)]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(rustango::Model)]
#[rustango(table = "erq_child")]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "erq_parent", on = "id")]
    pub parent_id: i64,
}

async fn rollbacks(admin: &Pool, db: &str) -> i64 {
    let _ = raw_execute_pool(admin, "SELECT pg_stat_clear_snapshot()", Vec::new()).await;
    let sql = format!("SELECT xact_rollback FROM pg_stat_database WHERE datname = '{db}'");
    let rows: Vec<(i64,)> = raw_query_pool(&sql, Vec::new(), admin)
        .await
        .expect("stats");
    rows.first().map_or(0, |(n,)| *n)
}

#[tokio::test]
async fn bootstrap_rerun_fails_no_statement() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL unset");
        return;
    };
    if !url.starts_with("postgres") {
        return;
    }
    let admin = Pool::connect(&url).await.expect("connect");
    let db = format!("rustango_erq_{}", std::process::id());
    let _ = raw_execute_pool(&admin, &format!("DROP DATABASE IF EXISTS {db}"), Vec::new()).await;
    raw_execute_pool(&admin, &format!("CREATE DATABASE {db}"), Vec::new())
        .await
        .expect("create database");
    let (base, _) = url.rsplit_once('/').expect("url has a database");
    // One connection, so one backend flushes every count below.
    let pg = rustango::sql::sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{base}/{db}"))
        .await
        .expect("connect fresh");
    let pool = Pool::from(pg);
    let models = [
        <Parent as rustango::core::Model>::SCHEMA,
        <Child as rustango::core::Model>::SCHEMA,
    ];
    for _ in 0..2 {
        rustango::testkit::migrate_framework(&pool)
            .await
            .expect("framework tables");
        rustango::testkit::create_tables(&pool, &models)
            .await
            .expect("model tables");
    }
    assert!(raw_execute_pool(&pool, "SELECT 1/0", Vec::new())
        .await
        .is_err());
    pool.close().await;
    let mut seen = 0;
    for _ in 0..100 {
        seen = rollbacks(&admin, &db).await;
        if seen >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = raw_execute_pool(&admin, &format!("DROP DATABASE IF EXISTS {db}"), Vec::new()).await;
    assert_eq!(
        seen,
        1,
        "only the marker failed; the re-run sent {} failing statement(s)",
        seen - 1
    );
}
