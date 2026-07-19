#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds; pattern is refutable on multi-backend builds.
//! Live regression — `migrate_registry_pool` against a SQLite registry.
//!
//! Post-`system-app` migrations: `migrate_registry_pool` no longer runs
//! hand-written `ensure_*` DDL. Instead it generates the framework's
//! registry-scope system migrations from the compiled models (into a
//! sibling `system/migrations/`) and applies them, creating the core
//! framework tables. This test proves that end-to-end on SQLite.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::migrate_registry_pool;

#[tokio::test]
async fn migrate_registry_pool_creates_framework_tables_on_sqlite() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).expect("create migrations dir");
    let dbpath = tmp.path().join("reg.db");
    let url = format!("sqlite:{}?mode=rwc", dbpath.display());
    let pool = Pool::connect(&url).await.expect("sqlite pool");

    // First run generates + applies the framework's system-app migrations.
    let applied = migrate_registry_pool(&pool, &dir)
        .await
        .expect("migrate_registry_pool");
    assert!(
        !applied.is_empty(),
        "framework system migrations should be generated + applied on first run"
    );

    let Pool::Sqlite(sq) = &pool else {
        panic!("expected sqlite pool");
    };
    // Registry-scope + shared framework tables now come from migrations,
    // not ensure_* DDL.
    for t in [
        "rustango_orgs",
        "rustango_operators",
        "rustango_audit_log",
        "rustango_content_types",
    ] {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(t)
                .fetch_one(sq)
                .await
                .expect("probe");
        assert_eq!(n, 1, "{t} must exist after migrate_registry_pool");
    }

    // Idempotent re-run — nothing new to apply (ledger-tracked).
    let applied2 = migrate_registry_pool(&pool, &dir)
        .await
        .expect("migrate_registry_pool idempotent");
    assert!(applied2.is_empty(), "re-run applies nothing");
    drop(tmp);
}
