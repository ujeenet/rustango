//! Every render golden applied live on its backend: each autocommit unit's
//! probe holds right after it runs, and the catalog then matches the
//! after-state (tables, columns, indexes, checks, FKs, PG pkey and sequence).
//!
//! Checked with the production catalog reader and probe.

use super::probe::Verdict;
use super::render::{Expect, Unit, UnitKind};
use super::render_tests::{cases, render, setup_state};
use super::state::{Backend, State};
use crate::sql::{raw_execute_pool, raw_execute_tx, raw_query_pool, Pool};

/// Child tables first, so MySQL drops them before what they reference.
const TABLES: &[&str] = &[
    "s3_child",
    "s3_t",
    "s3_defaults",
    "s3_adopt",
    "s3_parent",
    "s3_renamed",
    "s3_ext",
    "_rustango_rebuild_s3_child",
    "_rustango_rebuild_s3_t",
    "_rustango_rebuild_s3_parent",
];

async fn clean(pool: &Pool) {
    for t in TABLES {
        let _ = raw_execute_pool(pool, &format!("DROP TABLE IF EXISTS {t}"), Vec::new()).await;
    }
}

async fn exec(pool: &Pool, sql: &str) -> Result<(), String> {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .map(drop)
        .map_err(|e| format!("{sql}: {e}"))
}

/// Run one unit as the runner will: a transaction, statement by statement,
/// or a SQLite rebuild with FK enforcement off around it.
async fn run_unit(pool: &Pool, u: &Unit) -> Result<(), String> {
    match u.kind {
        UnitKind::Autocommit => {
            for s in &u.sql {
                exec(pool, s).await?;
            }
            Ok(())
        }
        UnitKind::Tx => {
            let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
            let mut tx = conn.begin().await.map_err(|e| e.to_string())?;
            for s in &u.sql {
                raw_execute_tx(&mut tx, s, Vec::new())
                    .await
                    .map_err(|e| format!("{s}: {e}"))?;
            }
            tx.commit().await.map_err(|e| e.to_string())
        }
        UnitKind::Rebuild => run_rebuild(pool, u).await,
    }
}

#[cfg(feature = "sqlite")]
#[allow(irrefutable_let_patterns)] // Pool has one variant in SQLite-only builds.
async fn run_rebuild(pool: &Pool, u: &Unit) -> Result<(), String> {
    let Pool::Sqlite(sq) = pool else {
        return Err("a rebuild unit is SQLite only".into());
    };
    let mut conn = crate::migrate::rebuild::RebuildConn::acquire(sq, true)
        .await
        .map_err(|e| e.to_string())?;
    let result: Result<(), String> = async {
        let mut tx = conn.begin().await.map_err(|e| e.to_string())?;
        for s in &u.sql {
            sqlx::query(s)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("{s}: {e}"))?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }
    .await;
    let finished = conn.finish().await.map_err(|e| e.to_string());
    result?;
    finished
}

#[cfg(not(feature = "sqlite"))]
async fn run_rebuild(_pool: &Pool, _u: &Unit) -> Result<(), String> {
    Err("a rebuild unit is SQLite only".into())
}

async fn strings(pool: &Pool, sql: &str) -> Vec<String> {
    let rows: Vec<(String,)> = raw_query_pool(sql, Vec::new(), pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.into_iter().map(|(s,)| s).collect()
}

fn backend(pool: &Pool) -> Backend {
    match pool.dialect().kind() {
        crate::sql::DialectKind::Postgres => Backend::Postgres,
        crate::sql::DialectKind::Mysql => Backend::Mysql,
        _ => Backend::Sqlite,
    }
}

/// The production probe: `Present` or the diff.
async fn probe(pool: &Pool, expects: &[Expect]) -> Result<(), String> {
    match super::probe::probe(pool, expects)
        .await
        .map_err(|e| e.to_string())?
    {
        Verdict::Present => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

async fn run(pool: &Pool, units: &[Unit], what: &str) {
    for u in units {
        run_unit(pool, u)
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        if let Some(expects) = &u.probe {
            probe(pool, expects)
                .await
                .unwrap_or_else(|m| panic!("{what}: probe after op {}: {m}", u.op));
        }
    }
}

/// The catalog matches `after` (the production reader, names exact), and
/// tables gone from state are gone.
async fn verify(pool: &Pool, name: &str, before: &State, after: &State) {
    let b = backend(pool);
    for (t, ts) in &after.tables {
        let table = Box::new((**ts).clone());
        probe(pool, &[Expect::Table { table }])
            .await
            .unwrap_or_else(|m| panic!("{name} {}: table {t}: {m}", b.as_str()));
        if b == Backend::Postgres {
            if let Some(seq) = &ts.pg_sequence {
                let col = ts.columns.iter().find(|c| c.auto).unwrap();
                let got = strings(
                    pool,
                    &format!("SELECT pg_get_serial_sequence('{t}', '{}')", col.column),
                )
                .await
                .remove(0);
                assert_eq!(
                    got.rsplit('.').next(),
                    Some(seq.as_str()),
                    "{name}: {t} sequence"
                );
            }
        }
    }
    for t in before
        .tables
        .keys()
        .filter(|t| !after.tables.contains_key(*t))
    {
        let gone = [Expect::NoTable {
            table: t.to_string(),
        }];
        probe(pool, &gone)
            .await
            .unwrap_or_else(|m| panic!("{name} {}: {t}: {m}", b.as_str()));
    }
}

async fn every_golden_applies_and_probes(pool: &Pool) {
    let b = backend(pool);
    for case in cases(b) {
        clean(pool).await;
        let (setup, s0) = setup_state(&case, b);
        run(pool, &setup, &format!("{} setup", case.name)).await;
        for s in &case.seed {
            exec(pool, s)
                .await
                .unwrap_or_else(|e| panic!("{} seed: {e}", case.name));
        }
        let (units, s1) = render(&s0, &case.ops, b).unwrap();
        run(pool, &units, case.name).await;
        verify(pool, case.name, &s0, &s1).await;
        if case.name == "add_column" {
            // The fill reached the old row.
            let int = if b == Backend::Mysql {
                "SIGNED"
            } else {
                "BIGINT"
            };
            let got: Vec<(i64,)> = raw_query_pool(
                &format!("SELECT CAST(score AS {int}) FROM s3_parent"),
                vec![],
                pool,
            )
            .await
            .unwrap();
            assert_eq!(got, vec![(0,)]);
        }
    }
    clean(pool).await;
}

macro_rules! per_backend {
    ($($name:ident: $backend:ident if $feature:literal),* $(,)?) => {
        $(
            #[cfg(feature = $feature)]
            #[tokio::test]
            async fn $name() {
                let _guard = crate::testkit::matrix::live_lock().lock().await;
                let Some(pool) = crate::testkit::matrix::Backend::$backend.pool().await else {
                    crate::testkit::matrix::Backend::$backend.skip_arm(stringify!($name));
                    return;
                };
                every_golden_applies_and_probes(&pool).await;
            }
        )*
    };
}

per_backend!(
    goldens_apply_live_postgres: Postgres if "postgres",
    goldens_apply_live_mysql: MySql if "mysql",
    goldens_apply_live_sqlite: Sqlite if "sqlite",
);
