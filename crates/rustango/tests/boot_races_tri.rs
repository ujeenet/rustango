//! Boot-time DDL and seeders run by several processes at once, as replicas
//! booting together do. Every call must succeed, on every backend.
//!
//! Postgres's `IF NOT EXISTS` is not atomic, so a racing `CREATE TABLE`
//! loses on a catalog row; a seeder that selects then inserts loses on
//! the unique key. Each scenario races a few callers over many rounds and
//! reports how many rounds had a failed call.

#![cfg(all(feature = "testkit", feature = "tenancy", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

const ROUNDS: usize = 20;
const RACERS: usize = 4;

#[derive(Model, Debug, Clone)]
#[rustango(table = "boot_race_row", app = "boot_races")]
pub struct BootRow {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

async fn setup(_: &Pool) {}

/// A database of the scenario's own on `backend`'s server, and its guard.
async fn private(backend: &Pool) -> (Pool, Box<dyn std::any::Any>) {
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    let var = match backend {
        #[cfg(feature = "postgres")]
        Pool::Postgres(_) => Some("DATABASE_URL"),
        #[cfg(feature = "mysql")]
        Pool::Mysql(_) => Some("MYSQL_TEST_URL"),
        _ => None,
    };
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    if let Some(var) = var {
        let db = scratch_db::ScratchDb::create(&std::env::var(var).unwrap(), "rustango_race").await;
        let pool = Pool::connect(db.url()).await.expect("scratch pool");
        return (pool, Box::new(db));
    }
    (backend.clone(), Box::new(()))
}

async fn exec(pool: &Pool, sql: &str) {
    rustango::sql::raw_execute_pool(pool, sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Run `call` from [`RACERS`] callers at once, [`ROUNDS`] times, each round
/// after `reset`. Panics with the failed-round count.
async fn race<F, Fut, T>(what: &str, pool: &Pool, reset: &[String], call: F) -> Vec<Vec<T>>
where
    F: Fn(Pool, usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut failed_rounds = 0;
    let mut first = None;
    let mut results = Vec::new();
    for round in 0..ROUNDS {
        for sql in reset {
            exec(pool, sql).await;
        }
        let calls = (0..RACERS).map(|_| call(pool.clone(), round));
        let outs = futures_util::future::join_all(calls).await;
        let mut oks = Vec::new();
        let mut failed = false;
        for out in outs {
            match out {
                Ok(v) => oks.push(v),
                Err(e) => {
                    failed = true;
                    first.get_or_insert(e);
                }
            }
        }
        failed_rounds += usize::from(failed);
        results.push(oks);
    }
    assert_eq!(
        failed_rounds,
        0,
        "{what}: {failed_rounds}/{ROUNDS} rounds had a failed call; first: {}",
        first.unwrap_or_default()
    );
    results
}

fn drop_sql(pool: &Pool, table: &str) -> String {
    format!("DROP TABLE IF EXISTS {}", pool.dialect().quote_ident(table))
}

#[cfg(feature = "cache")]
async fn cache_ensure_table_survives_a_concurrent_boot(backend: &Pool) {
    let (pool, _guard) = private(backend).await;
    let reset = [drop_sql(&pool, "boot_race_cache")];
    race(
        "DatabaseCache::ensure_table",
        &pool,
        &reset,
        |p, _| async move {
            rustango::cache::DatabaseCache::new(p, "boot_race_cache")
                .ensure_table()
                .await
                .map_err(|e| e.to_string())
        },
    )
    .await;
}

#[cfg(not(feature = "cache"))]
async fn cache_ensure_table_survives_a_concurrent_boot(_: &Pool) {}

#[cfg(not(feature = "runserver"))]
async fn bootstrap_survives_a_concurrent_boot(_: &Pool) {}

#[cfg(feature = "runserver")]
async fn bootstrap_survives_a_concurrent_boot(backend: &Pool) {
    let (pool, _guard) = private(backend).await;
    let reset = [drop_sql(&pool, BootRow::SCHEMA.table)];
    race("AppBuilder::bootstrap", &pool, &reset, |p, _| async move {
        rustango::server::AppBuilder::from_pool(p)
            .bootstrap(&[BootRow::SCHEMA])
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    })
    .await;
}

async fn content_types_seed_survives_a_concurrent_run(backend: &Pool) {
    let (pool, _guard) = private(backend).await;
    rustango::contenttypes::ensure_table(&pool)
        .await
        .expect("table");
    let reset = [format!(
        "DELETE FROM {}",
        pool.dialect().quote_ident("rustango_content_types")
    )];
    let inserted = race(
        "contenttypes::ensure_seeded",
        &pool,
        &reset,
        |p, _| async move {
            rustango::contenttypes::ensure_seeded(&p)
                .await
                .map_err(|e| e.to_string())
        },
    )
    .await;
    // Exactly one row per model, however the inserts were split.
    let models = rustango::core::inventory::iter::<rustango::core::ModelEntry>
        .into_iter()
        .filter(|e| e.schema.table != "rustango_content_types")
        .count();
    for round in inserted {
        assert_eq!(round.iter().sum::<usize>(), models, "{round:?}");
    }
}

async fn get_or_create_role_survives_concurrent_callers(backend: &Pool) {
    use rustango::tenancy::permissions::{get_or_create_role_pool, Role};
    let (pool, _guard) = private(backend).await;
    rustango::testkit::create_tables(&pool, &[Role::SCHEMA])
        .await
        .expect("roles table");
    let ids = race(
        "get_or_create_role_pool",
        &pool,
        &[],
        |p, round| async move {
            get_or_create_role_pool(&format!("pool{round}"), "", &p)
                .await
                .map_err(|e| e.to_string())
        },
    )
    .await;
    for round in ids {
        assert!(
            round.windows(2).all(|w| w[0] == w[1]),
            "one role: {round:?}"
        );
    }
}

/// The Postgres-typed entry point read a stale snapshot under the race.
async fn pg_get_or_create_role_survives_concurrent_callers(backend: &Pool) {
    #[cfg(feature = "postgres")]
    if let Pool::Postgres(_) = backend {
        let (pool, _guard) = private(backend).await;
        rustango::testkit::create_tables(&pool, &[rustango::tenancy::permissions::Role::SCHEMA])
            .await
            .expect("roles table");
        let Pool::Postgres(pg) = &pool else {
            unreachable!()
        };
        let ids = race("get_or_create_role", &pool, &[], |_, round| {
            let pg = pg.clone();
            async move {
                rustango::tenancy::permissions::get_or_create_role(&format!("pg{round}"), "", &pg)
                    .await
                    .map_err(|e| e.to_string())
            }
        })
        .await;
        for round in ids {
            assert!(
                round.windows(2).all(|w| w[0] == w[1]),
                "one role: {round:?}"
            );
        }
    }
    let _ = backend;
}

rustango::tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        cache_ensure_table_survives_a_concurrent_boot,
        bootstrap_survives_a_concurrent_boot,
        content_types_seed_survives_a_concurrent_run,
        get_or_create_role_survives_concurrent_callers,
        pg_get_or_create_role_survives_concurrent_callers,
    ],
}
