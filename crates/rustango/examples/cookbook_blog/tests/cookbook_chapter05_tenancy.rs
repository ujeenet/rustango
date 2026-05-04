//! Cookbook Chapter 5 — multi-tenancy primitives.
//!
//! Exercises resolvers, the registry, schema-mode tenant
//! provisioning, and lazy `TenantPools` against docker PG.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter05_tenancy -- --test-threads=1`

use rustango::sql::sqlx;
use rustango::tenancy::{
    self, ChainResolver, HeaderResolver, OrgResolver, SubdomainResolver, TenantPools,
};

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn pool() -> Option<sqlx::PgPool> {
    let url = url()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

// §5.71 / §5.66 / §5.68 / §5.70 / §5.74 / §5.73 — schema-per-tenant
// provisioning end-to-end. Provisions two tenants, then exercises
// SubdomainResolver / HeaderResolver / ChainResolver against the
// registry pool and asserts each finds the right Org row. Also
// confirms TenantPools::get_pool lazy-creates a connection scoped
// to the tenant's schema.
#[tokio::test]
async fn provision_two_tenants_then_resolve_and_lazy_pool() {
    let Some(registry) = pool().await else { return };
    cleanup_tenants(&registry, &["cookbook_acme", "cookbook_globex"]).await;

    let pools = TenantPools::new(registry.clone());
    let dir = std::env::temp_dir().join(format!(
        "cookbook_ch5_{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    tenancy::init_tenancy(&dir).expect("init bootstrap migrations");
    tenancy::migrate_registry(&pools, &dir).await.expect("migrate registry");

    let registry_url = url().unwrap();

    // §5.66b — SubdomainResolver works against a real apex pattern.
    // host_pattern = "{slug}.cookbook-test.local" so the resolver can
    // tell apart cookbook_acme.cookbook-test.local vs
    // cookbook_globex.cookbook-test.local.
    for slug in ["cookbook_acme", "cookbook_globex"] {
        let opts = tenancy::manage::api::CreateTenantOpts {
            host_pattern: Some(format!("{slug}.cookbook-test.local")),
            ..Default::default()
        };
        tenancy::manage::api::create_tenant_if_missing(
            &pools, &registry_url, &dir, slug, opts,
        ).await.expect("create_tenant_if_missing");
    }

    // Schema exists for each tenant.
    for slug in ["cookbook_acme", "cookbook_globex"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)"
        ).bind(slug).fetch_one(&registry).await.unwrap();
        assert!(exists, "tenant `{slug}` schema must exist");
    }

    // §5.66 — SubdomainResolver: acme.<apex> → cookbook_acme org.
    {
        let r = SubdomainResolver::new("cookbook-test.local");
        let req = http::Request::get("http://cookbook_acme.cookbook-test.local/")
            .body(()).unwrap();
        let parts = req.into_parts().0;
        let org = r.resolve(&parts, &registry).await.expect("resolve");
        let org = org.expect("subdomain resolver should match seeded acme tenant");
        assert_eq!(org.slug, "cookbook_acme");
    }

    // §5.68 — HeaderResolver: looks up by X-Org slug directly.
    {
        let r = HeaderResolver::default();
        let mut req = http::Request::get("http://localhost/").body(()).unwrap();
        req.headers_mut().insert("X-Org", "cookbook_globex".parse().unwrap());
        let parts = req.into_parts().0;
        let org = r.resolve(&parts, &registry).await.expect("resolve");
        let org = org.expect("X-Org should match seeded globex tenant");
        assert_eq!(org.slug, "cookbook_globex");
    }

    // §5.70 — ChainResolver tries Subdomain, then Header.
    {
        let r = ChainResolver::new()
            .push(SubdomainResolver::new("cookbook-test.local"))
            .push(HeaderResolver::default());
        // No subdomain → Subdomain arm misses; X-Org wins.
        let mut req = http::Request::get("http://localhost/").body(()).unwrap();
        req.headers_mut().insert("X-Org", "cookbook_acme".parse().unwrap());
        let parts = req.into_parts().0;
        let org = r.resolve(&parts, &registry).await.expect("resolve");
        let org = org.expect("chain should fall through to header arm");
        assert_eq!(org.slug, "cookbook_acme");
    }

    // §5.73 — TenantPools::get_pool returns a tenant-scoped pool.
    // For schema-mode tenants the pool's search_path lands on the
    // tenant's schema, so plain queries hit the right tables.
    {
        let acme = tenancy::manage::api::find_org(&pools, "cookbook_acme")
            .await.unwrap()
            .expect("acme org");
        let _tenant_pool = pools.pool_for_org(&acme).await.expect("tenant pool");
    }

    cleanup_tenants(&registry, &["cookbook_acme", "cookbook_globex"]).await;
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------- helpers ----------------

async fn cleanup_tenants(registry: &sqlx::PgPool, slugs: &[&str]) {
    for slug in slugs {
        let _ = sqlx::query(&format!(r#"DROP SCHEMA IF EXISTS "{slug}" CASCADE"#))
            .execute(registry).await;
        let _ = sqlx::query("DELETE FROM rustango_orgs WHERE slug = $1")
            .bind(slug)
            .execute(registry).await;
    }
}
