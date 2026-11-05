//! Backing test for `docs/tenancy.md` — the multi-tenancy guide. Compiles the
//! page's Rust examples and checks the `TenantSlug` binding it describes. No DB.
//!
//! Run: `cargo test -p rustango --test tenancy_guide_doc`

#![cfg(all(feature = "tenancy", feature = "cache"))]
#![allow(dead_code)]

use std::sync::Arc;

use axum::http::StatusCode;
use rustango::cache::{BoxedCache, InMemoryCache, ScopedCache};
use rustango::extractors::TenantScope;
use rustango::sql::sqlx::Database;
use rustango::sql::Auto;
use rustango::sql::CounterPool as _;
use rustango::tenancy::{TenantPoolsConfig, TenantSlug};
use rustango::Model;

// --- "Registry and tenant" example.

// One shared table, in the registry.
#[derive(Model)]
#[rustango(table = "billing_plans", scope = "registry")]
pub struct BillingPlan {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

// No scope: `tenant` is the default. Each tenant has its own table.
#[derive(Model)]
#[rustango(table = "invoices")]
pub struct Invoice {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub total_cents: i64,
}

// --- "How a request finds its tenant" example.

async fn invoice_count(t: TenantScope) -> Result<String, StatusCode> {
    // `t.org` is the Org the resolver found. `t.pool()` is its pool.
    let n = Invoice::objects()
        .count(t.pool())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(format!("{}: {n} invoices", t.org.slug))
}

// --- "Pools" example.

fn sized<DB: Database>(b: rustango::server::Builder<DB>) -> rustango::server::Builder<DB> {
    b.tenant_pools(TenantPoolsConfig {
        max_cached_database_pools: 200,
        database_pool_max_connections: 4,
        ..Default::default()
    })
}

// --- "Credentials and caches bind to the org id" example.

fn tenant_cache(cache: BoxedCache, t: &TenantScope) -> Option<ScopedCache> {
    // `None` only for an Org that was not read from the registry.
    let tenant = TenantSlug::of(&t.org)?;
    Some(ScopedCache::for_tenant(cache, &tenant))
}

#[test]
fn a_reused_slug_gets_a_new_cache_namespace() {
    let cache: BoxedCache = Arc::new(InMemoryCache::new());
    let old = ScopedCache::for_tenant(cache.clone(), &TenantSlug::new(7, "acme"));
    let new = ScopedCache::for_tenant(cache, &TenantSlug::new(8, "acme"));
    assert_eq!(old.prefix(), "tenant:#org:7:acme:");
    assert_eq!(new.prefix(), "tenant:#org:8:acme:");
    assert_ne!(TenantSlug::new(7, "acme"), TenantSlug::new(8, "acme"));
}

#[test]
fn model_scopes_are_as_the_page_says() {
    use rustango::core::{Model as _, ModelScope};
    assert_eq!(BillingPlan::SCHEMA.scope, ModelScope::Registry);
    assert_eq!(Invoice::SCHEMA.scope, ModelScope::Tenant);
    assert_eq!(rustango::tenancy::Org::SCHEMA.scope, ModelScope::Registry);
}
