//! `makemigrations` → `migrate` on every backend, for the autodetector
//! fixes in #1877–#1881.
//!
//! Each scenario writes real migration files from two snapshots and
//! applies them with the runner, so the ops, their order and their DDL
//! all reach a real server. The tables exist only as snapshots, so the
//! probes below are literal-only statements, quoted by the dialect.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::migrate::{
    make_migrations_from, migrate_pool_with_ledger, render_changes_split_with_dialect,
    SchemaChange, SchemaSnapshot,
};
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

fn fk(to: &str) -> Value {
    json!({"fk": {"kind": "fk", "to": to, "on": "id", "on_delete": "CASCADE"}})
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

/// `sql` with each `{}` replaced by the next name, quoted by the dialect.
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

/// Render `changes` for the pool's dialect and run them, as `unapply` would.
async fn apply_ops(pool: &Pool, changes: &[SchemaChange]) -> Result<(), String> {
    let b = render_changes_split_with_dialect(changes, &SchemaSnapshot::default(), pool.dialect())?;
    for sql in b.immediate.iter().chain(&b.deferred_fks) {
        raw_execute_pool(pool, sql, Vec::new())
            .await
            .map_err(|e| format!("{sql}: {e}"))?;
    }
    Ok(())
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

// ---------------------------------------------------------------- #1877

/// A new FK column references its target and a new unique column is
/// unique. Both used to land as a bare `ADD COLUMN`.
async fn add_column_keeps_fk_and_unique(pool: &Pool) {
    let (a, b) = ("mad_ac_author", "mad_ac_book");
    let chain = Chain::new(pool, "ac", &[b, a]).await;
    chain
        .step(
            pool,
            json!({"tables": [table(a, vec![id()]), table(b, vec![id()])]}),
        )
        .await
        .expect("initial");
    chain
        .step(
            pool,
            json!({"tables": [
                table(a, vec![id()]),
                table(b, vec![id(),
                    col("author_id", "i64", fk(a)),
                    col("email", "string", json!({"max_length": 64, "unique": true}))]),
            ]}),
        )
        .await
        .expect("AddColumn applies");

    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 1, 'x')",
        &[b, "id", "author_id", "email"],
    )
    .await
    .expect("a valid row");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 'x')",
            &[b, "id", "email"]
        )
        .await
        .is_err(),
        "the added column is UNIQUE on {}",
        pool.dialect().name()
    );
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (3, 99)",
            &[b, "id", "author_id"]
        )
        .await
        .is_err(),
        "the added column REFERENCES its target on {}",
        pool.dialect().name()
    );

    // The rollback: unapply inverts each AddColumn to a DropColumn.
    let drop = |c: &str| SchemaChange::DropColumn {
        table: b.into(),
        column: c.into(),
    };
    let undo = by_dialect! { pool,
        postgres => vec![drop("email"), drop("author_id")],
            because "DROP COLUMN takes its constraints with it",
        mysql => vec![drop("email")],
            because "MySQL refuses to drop a column an FK still uses (1828), a separate gap",
        sqlite => vec![drop("email"), drop("author_id")],
            because "the unique index is dropped first, or SQLite refuses the column",
    };
    apply_ops(pool, &undo.value).await.expect(undo.why);
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [unique_drops_on_long_names, add_column_keeps_fk_and_unique]
);
