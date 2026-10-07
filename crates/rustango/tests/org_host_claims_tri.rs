//! A host write waits for a concurrent claim of the same host, then
//! refuses it, instead of both tenants getting it (#2099).

#![cfg(all(feature = "tenancy", feature = "testkit"))]

use std::time::Duration;

use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
use rustango::tenancy::org_edit::{apply, OrgPatch};
use rustango::tenancy::{BackendKind, Org, OrgHost, StorageMode, TenancyError, TenantPools};
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    // Shared registry tables: create what is missing, never drop them.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn name(tag: &str) -> String {
    format!("claim-{tag}-{}", std::process::id())
}

async fn org(pool: &Pool, slug: &str) -> i64 {
    let mut org = Org {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        ..rustango::testkit::org()
    };
    org.save_pool(pool).await.expect("insert org");
    *org.id.get().expect("org id")
}

/// Run `write` while another transaction holds an uncommitted extra-host
/// row for `host` on `owner`, then commit that row.
async fn racing_an_extra_host<T, F>(pool: &Pool, owner: i64, host: &str, write: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut tx = rustango::sql::transaction_pool(pool).await.expect("begin");
    let mut row = OrgHost {
        id: Auto::Unset,
        org_id: owner,
        hostname: host.to_owned(),
        enabled: true,
        created_at: Auto::Unset,
    };
    row.insert_tx(&mut tx).await.expect("held host");
    let write = tokio::spawn(write);
    tokio::time::sleep(Duration::from_millis(300)).await;
    tx.commit().await.expect("commit");
    write.await.expect("join")
}

async fn base_host(pool: &Pool, slug: &str) -> Option<String> {
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await
        .expect("fetch org");
    orgs.into_iter().next().and_then(|o| o.host_pattern)
}

fn refused<T: std::fmt::Debug>(r: &Result<T, TenancyError>) {
    assert!(
        matches!(r, Err(TenancyError::Validation(m)) if m.contains("already used")),
        "{r:?}"
    );
}

async fn edit_refuses_a_host_claimed_meanwhile(pool: &Pool) {
    let owner = org(pool, &name("edit-a")).await;
    let slug = name("edit-b");
    org(pool, &slug).await;
    let host = format!("{}.example.test", name("edit"));
    let patch = OrgPatch {
        host_pattern: Some(host.clone()),
        ..OrgPatch::default()
    };
    let (p, s) = (pool.clone(), slug.clone());
    let r = racing_an_extra_host(
        pool,
        owner,
        &host,
        async move { apply(&p, &s, &patch).await },
    )
    .await;
    refused(&r);
    assert_eq!(base_host(pool, &slug).await, None);
}

async fn create_refuses_a_host_claimed_meanwhile(pool: &Pool) {
    let owner = org(pool, &name("create-a")).await;
    let slug = name("create-b");
    let host = format!("{}.example.test", name("create"));
    let opts = CreateTenantOpts {
        mode: StorageMode::Database,
        backend: BackendKind::Sqlite,
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some(host.clone()),
        no_migrate: true,
        ..CreateTenantOpts::default()
    };
    let (p, s) = (pool.clone(), slug.clone());
    let create = async move {
        let dir = std::path::Path::new("no-migrations");
        match p {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => create_tenant(&TenantPools::new(p), "", dir, &s, opts).await,
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => create_tenant(&TenantPools::new(p), "", dir, &s, opts).await,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => create_tenant(&TenantPools::new(p), "", dir, &s, opts).await,
        }
    };
    let r = racing_an_extra_host(pool, owner, &host, create).await;
    refused(&r);
    assert_eq!(base_host(pool, &slug).await, None);
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        edit_refuses_a_host_claimed_meanwhile,
        create_refuses_a_host_claimed_meanwhile,
    ],
}
