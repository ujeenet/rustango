//! Live regression for v0.34 slice 2 — `migrate_registry_pool`
//! against a SQLite registry. Proves the backend-agnostic registry
//! bootstrap (migration runner + audit table + contenttype seed)
//! works end-to-end without Postgres.
//!
//! The migration dir is intentionally empty — this test isn't about
//! the runner's row processing, it's about verifying the auxiliary
//! bootstrap (audit::ensure_table_pool, contenttypes::ensure_seeded_pool)
//! that runs unconditionally after the migration loop. Combined with
//! the existing contenttypes_pool_live + audit live tests this gives
//! coverage of the whole `migrate_registry_pool` happy path.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::migrate_registry_pool;

#[tokio::test]
async fn migrate_registry_pool_bootstraps_audit_and_contenttypes_on_sqlite() {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool"),
    );

    // Empty migration dir — exercises the bootstrap-only path
    // (audit::ensure_table_pool + contenttypes::ensure_seeded_pool).
    let tmp = tempfile::tempdir().expect("temp dir");
    let applied = migrate_registry_pool(&pool, tmp.path())
        .await
        .expect("migrate_registry_pool");
    assert_eq!(applied.len(), 0, "no migrations in empty dir");

    // Verify both auxiliary tables exist on the sqlite registry.
    if let Pool::Sqlite(sq) = &pool {
        let audit_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type='table' AND name='rustango_audit_log'",
        )
        .fetch_one(sq)
        .await
        .expect("audit probe");
        assert_eq!(audit_exists, 1, "audit table should exist");

        let ct_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type='table' AND name='rustango_content_types'",
        )
        .fetch_one(sq)
        .await
        .expect("contenttype probe");
        assert_eq!(ct_exists, 1, "contenttype table should exist");

        // Re-run for idempotency.
        let applied2 = migrate_registry_pool(&pool, tmp.path())
            .await
            .expect("migrate_registry_pool idempotent");
        assert_eq!(applied2.len(), 0);
    } else {
        panic!("expected sqlite pool");
    }
}
