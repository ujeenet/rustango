#![cfg(all(feature = "postgres", feature = "tenancy"))]
//! A schema named after the database role comes first in the default
//! `search_path`, so a registry table inside it hides the real one. The
//! CLI refuses to run against such a registry.
//!
//! Skipped silently when `DATABASE_URL` is unset.

#[path = "support/scratch_db.rs"]
mod scratch_db;

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;
use scratch_db::ScratchDb;

#[tokio::test]
async fn cli_refuses_a_registry_shadowed_by_a_role_schema() {
    let Ok(admin) = std::env::var("DATABASE_URL") else {
        return;
    };
    let db = ScratchDb::create(&admin, "rustango_role_shadow").await;
    let raw = sqlx::PgPool::connect(db.url()).await.expect("connect");
    let pools = TenantPools::<sqlx::Postgres>::new(raw.clone());
    let migrations = tempfile::tempdir().expect("migrations dir");
    let run = |args: &[&str]| {
        let (pools, url) = (&pools, db.url().to_owned());
        let dir = migrations.path().to_owned();
        let args = rustango::testkit::routed_args(args);
        async move {
            let mut buf: Vec<u8> = Vec::new();
            rustango::tenancy::manage::run_with_writer(pools, &url, &dir, args, &mut buf)
                .await
                .map_err(|e| e.to_string())
        }
    };
    run(&["migrate-registry"]).await.expect("registry");
    run(&["list-tenants"]).await.expect("no shadow yet");

    // The schema alone is fine; a registry table inside it is not.
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&raw)
        .await
        .unwrap();
    let schema = format!("\"{}\"", role.replace('"', "\"\""));
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&raw)
        .await
        .unwrap();
    run(&["list-tenants"]).await.expect("empty role schema");
    sqlx::query(&format!("CREATE TABLE {schema}.rustango_orgs (id INT)"))
        .execute(&raw)
        .await
        .unwrap();
    let err = run(&["list-tenants"]).await.expect_err("shadowed registry");
    assert!(err.contains(&role) && err.contains("rustango_orgs"), "{err}");
    drop(pools);
    raw.close().await;
}
