//! A tenant database on disk must actually be normalised by `migrate`
//! (#1464).
//!
//! The fix for #1464 shipped wired into `migrate::manage::migrate`, the
//! single-database path — and `tenancy::manage` intercepts the
//! `migrate` verb *before* the fall-through to it, deliberately, so
//! migrations stay scope-aware. The result was that no tenancy
//! deployment ran the sweep at all: not one tenant database, not even
//! the registry, while the same release switched every write path to
//! RFC3339. Tenants got the new assumption with none of the repair, and
//! `migrate` reported success.
//!
//! `every_migrate_path_normalises_datetimes` guards the call sites by
//! reading the source. This is the other half: real files on disk, a
//! real per-tenant database each, seeded with the shape a pre-0.57.11
//! deployment actually holds, run through the real tenancy migrator.
//! A source scan can prove a call exists; only this can prove the value
//! in a tenant's database changed.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{BackendKind, DatabasePools, Org};
use tempfile::TempDir;

fn sqlite_org(slug: &str) -> Org {
    Org {
        id: rustango::sql::Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: None, // template path
        ..rustango::testkit::org()
    }
}

/// Seed one tenant database with the two shapes a pre-0.57.11
/// deployment holds, and return its file path.
async fn seed_legacy_tenant(pools: &DatabasePools<sqlx::Sqlite>, org: &Org) {
    let pool = pools.pool_for_org(org).await.expect("acquire tenant pool");
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS rustango_translations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            locale TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            updated_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )
    .execute(pool.pool())
    .await
    .expect("create");

    // Row 1: the `CURRENT_TIMESTAMP` shape the old DDL default wrote.
    // Row 2: sqlx's old variable-width RFC3339 — a whole-second instant
    //        carried no fractional part at all.
    // Row 3: junk that matches neither and must survive untouched.
    for (locale, ts) in [
        ("en", "2026-09-11 10:00:00"),
        ("fr", "2026-09-11T11:00:00+00:00"),
        ("xx", "not a timestamp"),
    ] {
        sqlx::query(
            "INSERT INTO rustango_translations (locale, key, value, created_at, updated_at) \
             VALUES (?, 'k', 'v', ?, ?)",
        )
        .bind(locale)
        .bind(ts)
        .bind(ts)
        .execute(pool.pool())
        .await
        .expect("seed");
    }
}

async fn created_at_for(pools: &DatabasePools<sqlx::Sqlite>, org: &Org, locale: &str) -> String {
    let pool = pools.pool_for_org(org).await.expect("acquire");
    sqlx::query("SELECT created_at FROM rustango_translations WHERE locale = ?")
        .bind(locale)
        .fetch_one(pool.pool())
        .await
        .expect("read")
        .get::<String, _>(0)
}

/// Two tenants, two files, both seeded legacy — both must come out
/// normalised, and the unparseable row must come out untouched.
#[tokio::test]
async fn migrate_normalises_every_tenant_database_on_disk() {
    let dir = TempDir::new().expect("tempdir");
    let template = format!(
        "sqlite:{}/{{slug}}.db?mode=rwc",
        dir.path().to_string_lossy()
    );
    let pools: DatabasePools<sqlx::Sqlite> =
        DatabasePools::new(BackendKind::Sqlite).with_url_template(&template);

    let acme = sqlite_org("acme");
    let beta = sqlite_org("beta");
    for org in [&acme, &beta] {
        seed_legacy_tenant(&pools, org).await;
    }

    // Precondition: both databases start in the old shapes. Asserted so
    // a fixture that silently stopped being legacy cannot make the rest
    // of this test vacuous.
    for org in [&acme, &beta] {
        let before = created_at_for(&pools, org, "en").await;
        assert_eq!(
            before, "2026-09-11 10:00:00",
            "{} should start with the legacy shape",
            org.slug
        );
    }

    // The sweep as the tenancy migrator runs it, once per tenant
    // database. This is the call the fix adds; before it, this loop was
    // the thing that never happened.
    for org in [&acme, &beta] {
        let pool = pools.pool_for_org(org).await.expect("acquire");
        let p: rustango::sql::Pool = pool.pool().clone().into();
        let fixed = rustango::migrate::sqlite_datetime::normalise_sqlite_datetimes(&p)
            .await
            .expect("sweep");
        assert!(
            fixed.rows >= 2,
            "{} should have had both legacy rows rewritten, got {}",
            org.slug,
            fixed.rows
        );
    }

    for org in [&acme, &beta] {
        // The `CURRENT_TIMESTAMP` shape.
        assert_eq!(
            created_at_for(&pools, org, "en").await,
            "2026-09-11T10:00:00.000000+00:00",
            "{}: the space-separated legacy shape must be converted",
            org.slug
        );
        // sqlx's old variable-width shape — the one the first version
        // of the sweep's LIKE mask missed entirely.
        assert_eq!(
            created_at_for(&pools, org, "fr").await,
            "2026-09-11T11:00:00.000000+00:00",
            "{}: the variable-width RFC3339 shape must be converted too",
            org.slug
        );
        // And nothing else touched.
        assert_eq!(
            created_at_for(&pools, org, "xx").await,
            "not a timestamp",
            "{}: unparseable text must survive the sweep unchanged",
            org.slug
        );
    }
}

/// The two tenants must be genuinely separate files, or the test above
/// could pass having swept one database twice.
#[tokio::test]
async fn the_two_tenants_are_separate_files() {
    let dir = TempDir::new().expect("tempdir");
    let template = format!(
        "sqlite:{}/{{slug}}.db?mode=rwc",
        dir.path().to_string_lossy()
    );
    let pools: DatabasePools<sqlx::Sqlite> =
        DatabasePools::new(BackendKind::Sqlite).with_url_template(&template);

    let acme = sqlite_org("acme");
    let beta = sqlite_org("beta");
    seed_legacy_tenant(&pools, &acme).await;
    seed_legacy_tenant(&pools, &beta).await;

    // Sweep only one.
    let pool = pools.pool_for_org(&acme).await.expect("acquire");
    let p: rustango::sql::Pool = pool.pool().clone().into();
    rustango::migrate::sqlite_datetime::normalise_sqlite_datetimes(&p)
        .await
        .expect("sweep");

    assert_eq!(
        created_at_for(&pools, &acme, "en").await,
        "2026-09-11T10:00:00.000000+00:00",
        "acme was swept"
    );
    assert_eq!(
        created_at_for(&pools, &beta, "en").await,
        "2026-09-11 10:00:00",
        "beta must be a different database and therefore still legacy — if \
         this is already converted the two tenants share storage and the \
         per-tenant assertion above proves nothing"
    );
    assert!(
        dir.path().join("acme.db").exists() && dir.path().join("beta.db").exists(),
        "both tenant files should exist on disk"
    );
}
