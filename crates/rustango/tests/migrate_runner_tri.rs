//! The migrate runner's lock, its refusals, generated names and the FKs a
//! key change must move, on every backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::migrate::callbacks::MigrationCallbackFut;
use rustango::migrate::{
    file, make_migrations_from, migrate_pool_with_ledger, unapply_pool_with_ledger, CallbackOp,
    DataOp, Migration, Operation, SchemaChange,
};
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

fn slow(_pool: Pool) -> MigrationCallbackFut {
    Box::pin(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(())
    })
}
rustango::register_migration_callback!("mr_slow", slow);

fn id() -> Value {
    json!({"name": "id", "column": "id", "ty": "i64", "nullable": false,
           "primary_key": true, "auto": true})
}

fn col(name: &str, nullable: bool) -> Value {
    json!({"name": name, "column": name, "ty": "i64", "nullable": nullable,
           "primary_key": false})
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

fn write(dir: &std::path::Path, mig: &Migration) {
    file::write(&dir.join(format!("{}.json", mig.name)), mig).expect("write migration");
}

async fn exec(pool: &Pool, sql: &str) -> Result<(), String> {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A downgrade refused at its second op ran nothing: the first op's
/// column is still there and the migration still applied.
async fn refused_downgrade_runs_nothing(pool: &Pool) {
    refused_downgrade(pool, "mr_rd_item", true).await;
}

async fn refused_loose_downgrade_runs_nothing(pool: &Pool) {
    refused_downgrade(pool, "mr_rl_item", false).await;
}

async fn refused_downgrade(pool: &Pool, t: &str, atomic: bool) {
    let ledger = format!("{t}_ledger");
    let dir = fresh(pool, &[t], &ledger).await;
    let initial = make_migrations_from(
        dir.path(),
        &snap(vec![table(t, vec![id(), col("req", false)])]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    // Inverted, the AddColumn of `req` comes second, after `z` is dropped.
    let mig = Migration {
        name: "0002_swap".into(),
        prev: Some(initial.name.clone()),
        atomic,
        snapshot: snap(vec![table(t, vec![id(), col("z", true)])]),
        forward: vec![
            Operation::Schema(SchemaChange::DropColumn {
                table: t.into(),
                column: "req".into(),
            }),
            Operation::Schema(SchemaChange::AddColumn {
                table: t.into(),
                column: "z".into(),
            }),
        ],
        ..initial
    };
    write(dir.path(), &mig);
    migrate_pool_with_ledger(pool, dir.path(), &ledger)
        .await
        .expect("migrate");
    let err = unapply_pool_with_ledger(pool, dir.path(), &mig.name, &ledger)
        .await
        .expect_err("NOT NULL with no default is refused");
    assert!(err.to_string().contains("NOT NULL"), "{err}");
    let q = pool.dialect().quote_ident(t);
    exec(pool, &format!("INSERT INTO {q} (z) VALUES (1)"))
        .await
        .unwrap_or_else(|e| panic!("`z` was dropped on {}: {e}", pool.dialect().name()));
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE name = '0002_swap'",
        pool.dialect().quote_ident(&ledger)
    );
    let n: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .expect("ledger");
    assert_eq!(n[0].0, 1, "still applied");
    for d in [t, &ledger] {
        drop_table(pool, d).await;
    }
}

/// The URL of `pool`'s server, or a fresh SQLite file.
fn url_of(pool: &Pool, file: &std::path::Path) -> String {
    match pool.dialect().name() {
        "postgres" => std::env::var("DATABASE_URL").expect("DATABASE_URL"),
        "mysql" => std::env::var("MYSQL_TEST_URL").expect("MYSQL_TEST_URL"),
        _ => format!("sqlite://{}?mode=rwc", file.display()),
    }
}

async fn pool_of(url: &str, max: u32) -> Pool {
    let timeout = std::time::Duration::from_secs(5);
    match url.split(':').next() {
        #[cfg(feature = "postgres")]
        Some("postgres" | "postgresql") => sqlx_pg(url, max, timeout).await,
        #[cfg(feature = "mysql")]
        Some("mysql") => Pool::from(
            rustango::sql::sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(max)
                .acquire_timeout(timeout)
                .connect(url)
                .await
                .expect("connect"),
        ),
        #[cfg(feature = "sqlite")]
        Some("sqlite") => Pool::from(
            rustango::sql::sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(max)
                .acquire_timeout(timeout)
                .connect(url)
                .await
                .expect("connect"),
        ),
        _ => unreachable!("{url}"),
    }
}

#[cfg(feature = "postgres")]
async fn sqlx_pg(url: &str, max: u32, timeout: std::time::Duration) -> Pool {
    Pool::from(
        rustango::sql::sqlx::postgres::PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(timeout)
            .connect(url)
            .await
            .expect("connect"),
    )
}

/// A one-connection pool migrates: the lock no longer takes its only connection.
async fn one_connection_pool_migrates(pool: &Pool) {
    let t = "mr_one_item";
    let ledger = "mr_one_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let db = tempfile::tempdir().expect("tempdir");
    let one = pool_of(&url_of(pool, &db.path().join("one.db")), 1).await;
    make_migrations_from(dir.path(), &snap(vec![table(t, vec![id()])]), None)
        .expect("makemigrations")
        .expect("a migration");
    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        migrate_pool_with_ledger(&one, dir.path(), ledger),
    )
    .await
    .expect("no hang");
    ran.unwrap_or_else(|e| panic!("one connection on {}: {e}", pool.dialect().name()));
    drop_table(&one, t).await;
    drop_table(&one, ledger).await;
}

/// Two runs at once apply a non-atomic migration once, SQLite included.
async fn concurrent_runs_apply_once(pool: &Pool) {
    let t = "mr_cc_counter";
    let ledger = "mr_cc_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let db = tempfile::tempdir().expect("tempdir");
    let url = url_of(pool, &db.path().join("cc.db"));
    let (p1, p2) = (pool_of(&url, 5).await, pool_of(&url, 5).await);
    let initial = make_migrations_from(
        dir.path(),
        &snap(vec![table(t, vec![id(), col("n", true)])]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    migrate_pool_with_ledger(&p1, dir.path(), ledger)
        .await
        .expect("initial");
    let q = pool.dialect().quote_ident(t);
    write(
        dir.path(),
        &Migration {
            name: "0002_count".into(),
            prev: Some(initial.name.clone()),
            atomic: false,
            forward: vec![
                Operation::Callback(CallbackOp {
                    name: "mr_slow".into(),
                    reverse_name: None,
                }),
                Operation::Data(DataOp {
                    sql: format!("INSERT INTO {q} (n) VALUES (1)"),
                    reverse_sql: None,
                    reversible: false,
                }),
            ],
            ..initial
        },
    );
    let (r1, r2) = tokio::join!(
        migrate_pool_with_ledger(&p1, dir.path(), ledger),
        migrate_pool_with_ledger(&p2, dir.path(), ledger),
    );
    let name = pool.dialect().name();
    let n: Vec<(i64,)> =
        rustango::sql::raw_query_pool(&format!("SELECT COUNT(*) FROM {q}"), Vec::new(), &p1)
            .await
            .expect("count");
    assert_eq!(n[0].0, 1, "the RunSQL ran once on {name}");
    let applied = [r1, r2].map(|r| r.unwrap_or_else(|e| panic!("a run on {name}: {e}")));
    assert_eq!(applied.iter().map(Vec::len).sum::<usize>(), 1, "{applied:?}");
    drop_table(&p1, t).await;
    drop_table(&p1, ledger).await;
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [
        long_index_names_apply,
        referenced_key_widens,
        refused_downgrade_runs_nothing,
        refused_loose_downgrade_runs_nothing,
        one_connection_pool_migrates,
        concurrent_runs_apply_once,
    ]
);
