//! Live SQLite tests exercising the **file-per-tenant** template path.
//!
//! `database_pools_sqlite_live.rs` runs against `:memory:` for speed;
//! these tests use a `TempDir` so we can verify file persistence,
//! `{slug}` expansion against the real filesystem, and that two
//! tenants get genuinely separate databases (not just cache keys
//! against one shared in-memory DB).

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{BackendKind, DatabasePools, Org};
use tempfile::TempDir;

fn fake_sqlite_org(slug: &str) -> Org {
    Org {
        id: rustango::sql::Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: None, // <- template path
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
        sso_enabled: false,
        sso_provider: None,
        sso_issuer_url: None,
        sso_client_id: None,
        sso_secret_ref: None,
    }
}

#[tokio::test]
async fn template_creates_per_tenant_file_on_disk() {
    let dir = TempDir::new().expect("tempdir");
    let template = format!(
        "sqlite:{}/{{slug}}.db?mode=rwc",
        dir.path().to_string_lossy()
    );
    let pools: DatabasePools<sqlx::Sqlite> =
        DatabasePools::new(BackendKind::Sqlite).with_url_template(&template);

    let acme = fake_sqlite_org("acme");
    let beta = fake_sqlite_org("beta");

    // Acquire + write a marker row in each tenant's DB.
    for org in [&acme, &beta] {
        let pool = pools.pool_for_org(org).await.expect("acquire");
        sqlx::query("CREATE TABLE marker (slug TEXT NOT NULL)")
            .execute(pool.pool())
            .await
            .expect("create table");
        sqlx::query("INSERT INTO marker (slug) VALUES (?)")
            .bind(&org.slug)
            .execute(pool.pool())
            .await
            .expect("insert");
    }

    // Both files exist on disk.
    assert!(
        dir.path().join("acme.db").exists(),
        "acme.db should have been created by sqlite mode=rwc"
    );
    assert!(
        dir.path().join("beta.db").exists(),
        "beta.db should have been created by sqlite mode=rwc"
    );

    // Re-acquire and confirm each tenant sees only its own data.
    // Drop and rebuild via `invalidate` to force a fresh open of
    // the file (so we know the data persisted, not just the
    // in-memory pool's view of it).
    pools.invalidate("acme").await;
    pools.invalidate("beta").await;

    let acme_pool = pools.pool_for_org(&acme).await.expect("re-acquire acme");
    let row = sqlx::query("SELECT slug FROM marker")
        .fetch_one(acme_pool.pool())
        .await
        .expect("read acme marker");
    let slug: String = row.try_get("slug").expect("slug");
    assert_eq!(slug, "acme");

    let beta_pool = pools.pool_for_org(&beta).await.expect("re-acquire beta");
    let row = sqlx::query("SELECT slug FROM marker")
        .fetch_one(beta_pool.pool())
        .await
        .expect("read beta marker");
    let slug: String = row.try_get("slug").expect("slug");
    assert_eq!(slug, "beta");
}
