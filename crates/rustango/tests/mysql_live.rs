//! Live MySQL integration tests for the v0.23.0 bi-dialect surface.
//!
//! Activated when `MYSQL_TEST_URL` is set (e.g.
//! `mysql://rustango:rustango@127.0.0.1:3406/rustango_test`); otherwise
//! every test short-circuits to a single `eprintln!` and passes
//! trivially. Same style as the existing `s3_live_*.rs` tests.
//!
//! Setup the test container:
//!
//!   docker run -d --name rustango-mysql \
//!     -e MYSQL_ROOT_PASSWORD=rustango \
//!     -e MYSQL_DATABASE=rustango_test \
//!     -e MYSQL_USER=rustango \
//!     -e MYSQL_PASSWORD=rustango \
//!     -p 3406:3306 mysql:8.0
//!
//!   export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test
//!   cargo test -p rustango --features mysql --test mysql_live -- --nocapture
//!
//! Tests target a fresh schema each run — they `DROP TABLE IF EXISTS`
//! at the top of every test so re-running is idempotent.

#![cfg(feature = "mysql")]

use rustango::sql::{Auto, CounterPool, FetcherPool, Pool, PoolTx};
use rustango::Model;
use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets the shared schema
/// via `fresh_schema`; without serialization two tests racing on
/// `apply_all_pool` (which is not `IF NOT EXISTS` for framework models
/// like `rustango_content_types`) trip MySQL error 1050.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug)]
#[rustango(table = "live_users")]
#[allow(dead_code)]
pub struct LiveUser {
    #[rustango(primary_key)]
    id: Auto<i64>,
    #[rustango(max_length = 255)]
    name: String,
    is_active: bool,
}

#[derive(Model, Debug)]
#[rustango(table = "live_audited", audit(track = "name"))]
#[allow(dead_code)]
pub struct LiveAudited {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 255)]
    name: String,
}

async fn pool_or_skip() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    Some(
        Pool::connect(&url)
            .await
            .expect("connect to MYSQL_TEST_URL failed — is the container up?"),
    )
}

async fn fresh_schema(pool: &Pool) {
    use rustango::sql::raw_execute_pool;
    // Drop every registered framework + test model in one pass — this
    // covers `rustango_content_types` and any other inventory-registered
    // table that the hand-rolled drop list used to miss.
    rustango::migrate::drop_all_pool(pool)
        .await
        .expect("drop_all_pool");
    // Runtime side-tables (not registered models, so `drop_all_pool`
    // doesn't touch them): the audit log + the migration ledger. The
    // ledger drop matters across test files — without it, a prior
    // binary's bootstrap entries leak into `applied_set_pool` and trip
    // `migrate_pool_ledger_round_trips`'s "empty ledger" assertion.
    for tbl in ["rustango_audit_log", "__rustango_migrations__"] {
        let sql = format!("DROP TABLE IF EXISTS `{tbl}`");
        let _ = raw_execute_pool(pool, &sql, vec![]).await;
    }
    rustango::migrate::apply_all_pool(pool)
        .await
        .expect("apply_all_pool");
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("ensure_table_pool");
}

#[tokio::test]
async fn auto_pk_insert_pool_round_trips() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "alice".into(),
        is_active: true,
    };
    u.insert_pool(&pool).await.expect("insert_pool");
    // After insert, Auto<i64> PK should be populated from
    // LAST_INSERT_ID() — first row → 1.
    assert_eq!(u.id.get().copied(), Some(1));

    let n = LiveUser::objects()
        .count_pool(&pool)
        .await
        .expect("count_pool");
    assert_eq!(n, 1);
}

#[tokio::test]
async fn fetch_pool_round_trips_decoded_row() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "bob".into(),
        is_active: false,
    };
    u.insert_pool(&pool).await.expect("insert_pool");

    let users: Vec<LiveUser> = LiveUser::objects()
        .fetch_pool(&pool)
        .await
        .expect("fetch_pool");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "bob");
    assert!(!users[0].is_active);
    assert_eq!(users[0].id.get().copied(), Some(1));
}

#[tokio::test]
async fn save_pool_updates_existing_row() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "carol".into(),
        is_active: true,
    };
    u.insert_pool(&pool).await.expect("insert");
    u.name = "Carol".into();
    u.save_pool(&pool).await.expect("save_pool");

    let users: Vec<LiveUser> = LiveUser::objects().fetch_pool(&pool).await.expect("fetch");
    assert_eq!(users[0].name, "Carol");
}

#[tokio::test]
async fn delete_pool_removes_row() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    let mut u = LiveUser {
        id: Auto::Unset,
        name: "doomed".into(),
        is_active: true,
    };
    u.insert_pool(&pool).await.expect("insert");
    let affected = u.delete_pool(&pool).await.expect("delete_pool");
    assert_eq!(affected, 1);
    let n = LiveUser::objects().count_pool(&pool).await.expect("count");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn audited_save_pool_emits_diff_audit_row() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    // Insert (creates entry) — note: insert_pool on audited path
    // emits a Create audit row too.
    let mut a = LiveAudited {
        id: 100,
        name: "before".into(),
    };
    a.insert_pool(&pool).await.expect("insert audited");

    // UPDATE the tracked column. Diff audit row should land in
    // rustango_audit_log with operation='update' and changes
    // containing { "name": { "before": "before", "after": "after" } }.
    a.name = "after".into();
    a.save_pool(&pool).await.expect("save_pool audited");

    // Read the audit table directly — confirm exactly one
    // Update entry whose JSON changes captures the field delta.
    use sqlx::Row as _;
    let my = pool.as_mysql().expect("pool is mysql");
    let row = sqlx::query(
        r#"SELECT operation, changes FROM `rustango_audit_log`
           WHERE entity_table = 'live_audited' AND operation = 'update'
           ORDER BY id DESC LIMIT 1"#,
    )
    .fetch_one(my)
    .await
    .expect("audit row");
    let op: String = row.try_get("operation").expect("operation col");
    assert_eq!(op, "update");
    let changes: serde_json::Value = row
        .try_get::<sqlx::types::Json<serde_json::Value>, _>("changes")
        .expect("changes col")
        .0;
    let name_diff = &changes["name"];
    assert_eq!(name_diff["before"], serde_json::json!("before"));
    assert_eq!(name_diff["after"], serde_json::json!("after"));
}

#[tokio::test]
async fn transaction_pool_commit_persists() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    let tx = rustango::sql::transaction_pool(&pool)
        .await
        .expect("begin tx");
    match tx {
        PoolTx::Mysql(mut t) => {
            sqlx::query("INSERT INTO `live_users` (`name`, `is_active`) VALUES (?, ?)")
                .bind("inside-tx")
                .bind(true)
                .execute(&mut *t)
                .await
                .expect("insert in tx");
            // Commit via the wrapper.
            PoolTx::Mysql(t).commit().await.expect("commit");
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("test runs with mysql feature"),
    }

    let n = LiveUser::objects().count_pool(&pool).await.expect("count");
    assert_eq!(n, 1);
}

#[tokio::test]
async fn migrate_pool_ledger_round_trips() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let _g = live_lock().lock().await;
    fresh_schema(&pool).await;

    // ensure_ledger_pool is idempotent + creates the
    // __rustango_migrations__ table with the right column types
    // (DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) on MySQL).
    rustango::migrate::ensure_ledger_pool(&pool)
        .await
        .expect("ensure_ledger_pool");
    let applied = rustango::migrate::applied_set_pool(&pool)
        .await
        .expect("applied_set_pool");
    assert!(applied.is_empty());
}
