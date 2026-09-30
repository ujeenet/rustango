//! `makemigrations` → `migrate` on every backend, for the autodetector
//! fixes in #1877–#1881.
//!
//! Each scenario writes real migration files from two snapshots and
//! applies them with the runner, so the ops, their order and their DDL
//! all reach a real server. The tables exist only as snapshots, so the
//! probes below are literal-only statements, quoted by the dialect.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::migrate::{make_migrations_from, migrate_pool_with_ledger, SchemaSnapshot};
use rustango::sql::{raw_execute_pool, Pool};
use rustango::testkit::matrix::drop_table;
use rustango::{by_dialect, tri_dialect_test};
use serde_json::{json, Value};

async fn no_setup(_pool: &Pool) {}

fn id() -> Value {
    json!({"name": "id", "column": "id", "ty": "i64", "nullable": false,
           "primary_key": true, "auto": true})
}

/// A nullable, non-PK column; `extra` overrides any key.
fn col(name: &str, ty: &str, extra: Value) -> Value {
    let mut f = json!({"name": name, "column": name, "ty": ty, "nullable": true,
                       "primary_key": false});
    if let Value::Object(m) = extra {
        f.as_object_mut().unwrap().extend(m);
    }
    f
}

fn table(name: &str, fields: Vec<Value>) -> Value {
    json!({"name": name, "model": name, "fields": fields})
}

fn snap(v: Value) -> SchemaSnapshot {
    serde_json::from_value(v).expect("snapshot JSON")
}

/// A migrations dir and a ledger of its own, over tables dropped first.
struct Chain {
    dir: tempfile::TempDir,
    ledger: String,
}

impl Chain {
    /// `tables` child-first, so a persistent server drops them cleanly.
    async fn new(pool: &Pool, tag: &str, tables: &[&str]) -> Self {
        for t in tables {
            drop_table(pool, t).await;
        }
        let ledger = format!("mad_ledger_{tag}");
        drop_table(pool, &ledger).await;
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            ledger,
        }
    }

    /// `makemigrations` against `current`, then `migrate`.
    async fn step(&self, pool: &Pool, current: Value) -> Result<(), String> {
        make_migrations_from(self.dir.path(), &snap(current), None)
            .map_err(|e| e.to_string())?
            .expect("the snapshot changed, so a migration is written");
        migrate_pool_with_ledger(pool, self.dir.path(), &self.ledger)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// `sql` with `{name}` placeholders quoted by the pool's dialect.
fn q(pool: &Pool, sql: &str, names: &[&str]) -> String {
    let mut out = sql.to_owned();
    for n in names {
        out = out.replacen("{}", &pool.dialect().quote_ident(n), 1);
    }
    out
}

async fn exec(pool: &Pool, sql: &str, names: &[&str]) -> Result<(), String> {
    raw_execute_pool(pool, &q(pool, sql, names), Vec::new())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- #1880

/// Dropping `unique` on a table+column too long for a plain
/// `{table}_{column}_key`: PG shortened the inline name, so the DROP
/// used to miss it.
async fn unique_drops_on_long_names(pool: &Pool) {
    let t = "mad_un_subscription_notification_preferences";
    let c = "primary_contact_email_address";
    let chain = Chain::new(pool, "un", &[t]).await;
    let with = |unique: bool| {
        json!({"tables": [table(t, vec![id(),
            col(c, "string", json!({"max_length": 100, "unique": unique}))])]})
    };
    chain
        .step(pool, with(true))
        .await
        .expect("CREATE TABLE with a named UNIQUE");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'a')",
        &[t, "id", c],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 'a')",
            &[t, "id", c]
        )
        .await
        .is_err(),
        "the column is UNIQUE on {}",
        pool.dialect().name()
    );

    let drop = chain.step(pool, with(false)).await;
    let runs = by_dialect! { pool,
        postgres => true, because "AlterColumnUnique renders on Postgres",
        mysql => false, because "AlterColumn* is refused at render until #559",
        sqlite => false, because "AlterColumn* is refused at render until #559",
    };
    if !runs.value {
        let err = drop.expect_err(runs.why);
        assert!(err.contains("AlterColumnUnique"), "{err}");
        return;
    }
    drop.expect("dropping UNIQUE finds the constraint by name");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (2, 'a')",
        &[t, "id", c],
    )
    .await
    .expect("no longer unique");
}

tri_dialect_test!(setup: no_setup, scenarios: [unique_drops_on_long_names]);
