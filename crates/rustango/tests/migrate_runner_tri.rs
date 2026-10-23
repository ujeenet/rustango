//! The migrate runner's lock, its refusals, generated names and the FKs a
//! key change must move, on every backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::migrate::{make_migrations_from, migrate_pool_with_ledger, unapply_pool_with_ledger};
use rustango::sql::{raw_execute_pool, Pool};
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::{tri_dialect_test, Model};
use serde_json::{json, Value};

async fn no_setup(_pool: &Pool) {}

/// Two composite indexes whose derived names share their first 63 bytes,
/// and a field index whose derived name is longer than MySQL's 64.
#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_subscription_notification_preference_entries")]
#[rustango(index("primary_contact_email_address, secondary_a"))]
#[rustango(index("primary_contact_email_address, secondary_b"))]
#[allow(dead_code)]
pub struct LongIdx {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(index, max_length = 40)]
    pub primary_contact_email_address: String,
    #[rustango(max_length = 10)]
    pub secondary_a: String,
    #[rustango(max_length = 10)]
    pub secondary_b: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_ck_a")]
#[rustango(check(name = "mr_ck_positive", expr = "n >= 0"))]
#[allow(dead_code)]
pub struct CkA {
    #[rustango(primary_key)]
    pub id: i64,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_ck_b")]
#[rustango(check(name = "mr_ck_positive", expr = "n >= 0"))]
#[allow(dead_code)]
pub struct CkB {
    #[rustango(primary_key)]
    pub id: i64,
    pub n: i64,
}

#[test]
fn derived_index_names_fit_and_differ() {
    let names: Vec<&str> = LongIdx::SCHEMA.indexes.iter().map(|i| i.name).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    for n in &names {
        assert!(n.len() <= 63, "`{n}` is {} bytes", n.len());
    }
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{names:?}");
}

/// The snapshot keys CHECKs by name, so one of the two tables lost its.
#[test]
fn check_name_on_two_tables_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = rustango::migrate::make_migrations(dir.path(), None)
        .expect_err("a CHECK name on two tables");
    let msg = err.to_string();
    assert!(
        msg.contains("mr_ck_positive") && msg.contains("mr_ck_a") && msg.contains("mr_ck_b"),
        "{msg}"
    );
}

/// The model's indexes create on every backend.
async fn long_index_names_apply(pool: &Pool) {
    fresh_table::<LongIdx>(pool).await;
    drop_table(pool, LongIdx::SCHEMA.table).await;
}

/// Widening a PK that another table's FK references, with the FK column.
async fn referenced_key_widens(pool: &Pool) {
    let (a, b) = ("mr_wk_author", "mr_wk_book");
    let ledger = "mr_wk_ledger";
    let dir = fresh(pool, &[b, a], ledger).await;
    let with = |ty: &str| {
        let key = |name: &str, fk: Option<&str>| {
            let mut f = json!({"name": name, "column": name, "ty": ty,
                "nullable": false, "primary_key": fk.is_none()});
            if let Some(to) = fk {
                f["fk"] = json!({"kind": "fk", "to": to, "on": "id", "on_delete": "CASCADE"});
            }
            f
        };
        let book_id = json!({"name": "id", "column": "id", "ty": "i64",
            "nullable": false, "primary_key": true});
        snap(vec![
            table(a, vec![key("id", None)]),
            table(b, vec![book_id, key("author_id", Some(a))]),
        ])
    };
    let step = |ty: &str| {
        make_migrations_from(dir.path(), &with(ty), None)
            .expect("makemigrations")
            .expect("a migration")
    };
    step("i32");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .expect("initial");
    let widen = step("i64");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("widening on {}: {e}", pool.dialect().name()));
    let d = pool.dialect();
    let (qa, qb) = (d.quote_ident(a), d.quote_ident(b));
    exec(pool, &format!("INSERT INTO {qa} (id) VALUES (3000000000)"))
        .await
        .expect("the PK is BIGINT");
    exec(
        pool,
        &format!("INSERT INTO {qb} (id, author_id) VALUES (1, 3000000000)"),
    )
    .await
    .expect("the FK column is BIGINT");
    assert!(
        exec(pool, &format!("INSERT INTO {qb} (id, author_id) VALUES (2, 7)"))
            .await
            .is_err(),
        "the FK is back on {}",
        d.name()
    );
    exec(pool, &format!("DELETE FROM {qb}")).await.unwrap();
    exec(pool, &format!("DELETE FROM {qa}")).await.unwrap();
    unapply_pool_with_ledger(pool, dir.path(), &widen.name, ledger)
        .await
        .unwrap_or_else(|e| panic!("narrowing on {}: {e}", d.name()));
    for t in [b, a, ledger] {
        drop_table(pool, t).await;
    }
}

fn snap(tables: Vec<Value>) -> rustango::migrate::SchemaSnapshot {
    serde_json::from_value(json!({"tables": tables})).expect("snapshot")
}

fn table(name: &str, fields: Vec<Value>) -> Value {
    json!({"name": name, "model": name, "fields": fields})
}

/// `tables` and `ledger` dropped, and an empty migrations dir.
async fn fresh(pool: &Pool, tables: &[&str], ledger: &str) -> tempfile::TempDir {
    for t in tables.iter().chain([&ledger]) {
        drop_table(pool, t).await;
    }
    tempfile::tempdir().expect("tempdir")
}

async fn exec(pool: &Pool, sql: &str) -> Result<(), String> {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [
        long_index_names_apply,
        referenced_key_widens,
    ]
);
