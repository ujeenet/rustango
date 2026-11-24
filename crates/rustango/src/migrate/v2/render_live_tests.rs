//! Every render golden applied live on its backend: each autocommit unit's
//! probe holds right after it runs, and the catalog then matches the
//! after-state (tables, columns, indexes, checks, FKs, PG pkey and sequence).
//!
//! The catalog queries here are test-side; the S6 catalog reader replaces
//! them when the runner lands.

use std::collections::BTreeSet;

use super::render::{Expect, Unit, UnitKind};
use super::render_tests::{cases, render, setup_state};
use super::state::{Backend, State, TableState};
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

/// The catalog's view of one table, as far as 1b checks it.
#[derive(Debug, PartialEq, Eq)]
struct Live {
    columns: BTreeSet<(String, bool)>,
    indexes: BTreeSet<String>,
    checks: BTreeSet<String>,
    fks: BTreeSet<String>,
    pg_pkey: Option<String>,
}

/// `CONSTRAINT "<name>" <kind>` names in SQLite's stored CREATE TABLE text.
fn sqlite_named(sql: &str, kind: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for part in sql.split("CONSTRAINT \"").skip(1) {
        if let Some((name, rest)) = part.split_once('"') {
            if rest.trim_start().starts_with(kind) {
                out.insert(name.to_owned());
            }
        }
    }
    out
}

async fn read(pool: &Pool, t: &str) -> Option<Live> {
    if !crate::migrate::try_table_exists_here(pool, t)
        .await
        .unwrap()
    {
        return None;
    }
    let b = backend(pool);
    let pairs = |rows: Vec<(String, String)>| -> BTreeSet<(String, bool)> {
        rows.into_iter().map(|(c, n)| (c, n == "YES")).collect()
    };
    Some(match b {
        Backend::Postgres => {
            let cols: Vec<(String, String)> = raw_query_pool(
                &format!(
                    "SELECT column_name::text, is_nullable::text FROM information_schema.columns \
                     WHERE table_schema = current_schema() AND table_name = '{t}'"
                ),
                Vec::new(),
                pool,
            )
            .await
            .unwrap();
            let con = |kind: &str| {
                format!(
                    "SELECT c.conname::text FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
                     JOIN pg_namespace n ON n.oid = t.relnamespace \
                     WHERE n.nspname = current_schema() AND t.relname = '{t}' AND c.contype = '{kind}'"
                )
            };
            Live {
                columns: pairs(cols),
                indexes: strings(
                    pool,
                    &format!(
                        "SELECT i.relname::text FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
                         JOIN pg_class t ON t.oid = x.indrelid JOIN pg_namespace n ON n.oid = t.relnamespace \
                         WHERE n.nspname = current_schema() AND t.relname = '{t}' AND NOT x.indisprimary \
                         AND x.indisvalid"
                    ),
                )
                .await
                .into_iter()
                .collect(),
                checks: strings(pool, &con("c")).await.into_iter().collect(),
                fks: strings(pool, &con("f")).await.into_iter().collect(),
                pg_pkey: strings(pool, &con("p")).await.into_iter().next(),
            }
        }
        Backend::Mysql => {
            let cols: Vec<(String, String)> = raw_query_pool(
                &format!(
                    "SELECT CAST(column_name AS CHAR), CAST(is_nullable AS CHAR) \
                     FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = '{t}'"
                ),
                Vec::new(),
                pool,
            )
            .await
            .unwrap();
            let con = |kind: &str| {
                format!(
                    "SELECT CAST(constraint_name AS CHAR) FROM information_schema.table_constraints \
                     WHERE table_schema = DATABASE() AND table_name = '{t}' AND constraint_type = '{kind}'"
                )
            };
            Live {
                columns: pairs(cols),
                indexes: strings(
                    pool,
                    &format!(
                        "SELECT DISTINCT CAST(index_name AS CHAR) FROM information_schema.statistics \
                         WHERE table_schema = DATABASE() AND table_name = '{t}' AND index_name <> 'PRIMARY'"
                    ),
                )
                .await
                .into_iter()
                .collect(),
                checks: strings(pool, &con("CHECK")).await.into_iter().collect(),
                fks: strings(pool, &con("FOREIGN KEY")).await.into_iter().collect(),
                pg_pkey: None,
            }
        }
        Backend::Sqlite => {
            let cols: Vec<(String, String)> = raw_query_pool(
                &format!(
                    "SELECT name, CASE WHEN \"notnull\" = 1 OR pk = 1 THEN 'NO' ELSE 'YES' END \
                     FROM pragma_table_info('{t}')"
                ),
                Vec::new(),
                pool,
            )
            .await
            .unwrap();
            let sql = strings(
                pool,
                &format!("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = '{t}'"),
            )
            .await
            .remove(0);
            Live {
                columns: pairs(cols),
                indexes: strings(
                    pool,
                    &format!(
                        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = '{t}' \
                         AND name NOT LIKE 'sqlite_autoindex%'"
                    ),
                )
                .await
                .into_iter()
                .collect(),
                checks: sqlite_named(&sql, "CHECK"),
                fks: sqlite_named(&sql, "FOREIGN KEY"),
                pg_pkey: None,
            }
        }
    })
}

fn backend(pool: &Pool) -> Backend {
    match pool.dialect().kind() {
        crate::sql::DialectKind::Postgres => Backend::Postgres,
        crate::sql::DialectKind::Mysql => Backend::Mysql,
        _ => Backend::Sqlite,
    }
}

/// What the catalog should hold for `t` on `b`.
fn wanted(t: &TableState, b: Backend) -> Live {
    Live {
        columns: t
            .columns
            .iter()
            .map(|c| (c.column.clone(), c.nullable))
            .collect(),
        indexes: t
            .indexes
            .iter()
            .filter(|i| !i.skipped_on.contains(&b))
            .map(|i| i.name.clone())
            .collect(),
        checks: t.checks.iter().map(|c| c.name.clone()).collect(),
        fks: t.fks.iter().map(|f| f.name.clone()).collect(),
        pg_pkey: if b == Backend::Postgres {
            t.pg_pkey.clone()
        } else {
            None
        },
    }
}

/// The probe of one autocommit unit, against the catalog.
async fn probe(pool: &Pool, e: &Expect) -> Result<(), String> {
    let b = backend(pool);
    let fail = |what: String| Err(what);
    match e {
        Expect::Table(t) => {
            let live = read(pool, &t.name).await;
            if live.as_ref() != Some(&wanted(t, b)) {
                return fail(format!("table {}: {live:?}", t.name));
            }
        }
        Expect::NoTable(t) => {
            if read(pool, t).await.is_some() {
                return fail(format!("table {t} still there"));
            }
        }
        Expect::Column { table: t, column } => {
            let live = read(pool, t).await.ok_or(format!("no table {t}"))?;
            if !live
                .columns
                .contains(&(column.column.clone(), column.nullable))
            {
                return fail(format!(
                    "{t}.{} not as expected: {:?}",
                    column.column, live.columns
                ));
            }
        }
        Expect::NoColumn { table: t, column } => {
            let live = read(pool, t).await.ok_or(format!("no table {t}"))?;
            if live.columns.iter().any(|(c, _)| c == column) {
                return fail(format!("{t}.{column} still there"));
            }
        }
        Expect::Index { table: t, index } => {
            let live = read(pool, t).await.ok_or(format!("no table {t}"))?;
            if !index.skipped_on.contains(&b) && !live.indexes.contains(&index.name) {
                return fail(format!("index {} missing or invalid", index.name));
            }
        }
        Expect::NoIndex { table: t, name } => {
            if read(pool, t)
                .await
                .is_some_and(|l| l.indexes.contains(name))
            {
                return fail(format!("index {name} still there"));
            }
        }
        Expect::Check { table: t, check } => {
            if !read(pool, t)
                .await
                .is_some_and(|l| l.checks.contains(&check.name))
            {
                return fail(format!("check {} missing", check.name));
            }
        }
        Expect::NoCheck { table: t, name } => {
            if read(pool, t).await.is_some_and(|l| l.checks.contains(name)) {
                return fail(format!("check {name} still there"));
            }
        }
        Expect::Fk { table: t, fk } => {
            if !read(pool, t)
                .await
                .is_some_and(|l| l.fks.contains(&fk.name))
            {
                return fail(format!("FK {} missing", fk.name));
            }
        }
        Expect::NoFk { table: t, name } => {
            if read(pool, t).await.is_some_and(|l| l.fks.contains(name)) {
                return fail(format!("FK {name} still there"));
            }
        }
        Expect::Sql(sql) => exec(pool, sql).await?,
        Expect::Idempotent => {}
    }
    Ok(())
}

async fn run(pool: &Pool, units: &[Unit], what: &str) {
    for u in units {
        run_unit(pool, u)
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        for e in u.probe.iter().flatten() {
            probe(pool, e)
                .await
                .unwrap_or_else(|m| panic!("{what}: probe after op {}: {m}", u.op));
        }
    }
}

/// The catalog matches `after`, and tables gone from state are gone.
async fn verify(pool: &Pool, name: &str, before: &State, after: &State) {
    let b = backend(pool);
    for (t, ts) in &after.tables {
        let live = read(pool, t).await;
        assert_eq!(
            live,
            Some(wanted(ts, b)),
            "{name} {}: table {t}",
            b.as_str()
        );
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
        assert!(
            read(pool, t).await.is_none(),
            "{name} {}: {t} still exists",
            b.as_str()
        );
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
