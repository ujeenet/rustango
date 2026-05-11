#![cfg(feature = "tenancy")]
//! Live tests for the OrgResolver chain. Spins up a registry with
//! three orgs in mixed config (subdomain, header-only, port-based)
//! and exercises each resolver + the chain composition.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use http::request::Parts;
use http::{HeaderName, HeaderValue, Request};
use rustango::migrate;
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{
    ChainResolver, HeaderResolver, Org, OrgResolver, PathPrefixResolver, PortResolver, StorageMode,
    SubdomainResolver,
};

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        sqlx::PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL"),
    )
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Insert three orgs in different routing modes.
async fn seed_orgs(pool: &sqlx::PgPool) {
    let mut acme = Org {
        id: Auto::default(),
        slug: "acme".into(),
        display_name: "ACME".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("acme".into()),
        host_pattern: Some("acme.app.test".into()),
        port: None,
        path_prefix: Some("/acme".into()),
        active: true,
        created_at: now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    acme.insert(pool).await.unwrap();

    // Header-only: no host_pattern, just the slug.
    let mut globex = Org {
        id: Auto::default(),
        slug: "globex".into(),
        display_name: "Globex".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some("env://TENANT_GLOBEX_DB_URL".into()),
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    globex.insert(pool).await.unwrap();

    // Port-based: dedicated port 9001, no host or path.
    let mut initech = Org {
        id: Auto::default(),
        slug: "initech".into(),
        display_name: "Initech".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("initech".into()),
        host_pattern: None,
        port: Some(9001),
        path_prefix: None,
        active: true,
        created_at: now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    initech.insert(pool).await.unwrap();

    // Inactive — every resolver should skip.
    let mut frozen = Org {
        id: Auto::default(),
        slug: "frozen".into(),
        display_name: "Frozen".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("frozen".into()),
        host_pattern: Some("frozen.app.test".into()),
        port: None,
        path_prefix: None,
        active: false,
        created_at: now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    frozen.insert(pool).await.unwrap();
}

fn parts_with_host(host: &str) -> Parts {
    let mut req = Request::builder()
        .uri(format!("http://{host}/"))
        .body(())
        .unwrap();
    req.headers_mut()
        .insert(http::header::HOST, HeaderValue::from_str(host).unwrap());
    req.into_parts().0
}

fn parts_with_uri(uri: &str) -> Parts {
    Request::builder().uri(uri).body(()).unwrap().into_parts().0
}

fn parts_with_header(header: &'static str, value: &str) -> Parts {
    let mut req = Request::builder()
        .uri("http://example.test/")
        .body(())
        .unwrap();
    req.headers_mut().insert(
        HeaderName::from_static(header),
        HeaderValue::from_str(value).unwrap(),
    );
    req.into_parts().0
}

#[tokio::test]
async fn subdomain_resolver_matches_host_pattern() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = SubdomainResolver::new("app.test");
    let parts = parts_with_host("acme.app.test");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert!(org.is_some(), "acme.app.test should resolve to acme");
    assert_eq!(org.unwrap().slug, "acme");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn subdomain_resolver_apex_returns_none() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = SubdomainResolver::new("app.test");
    // Bare apex: no tenant. Apex hosts only operator UI per the
    // v0.5 design — resolver returns Ok(None).
    let parts = parts_with_host("app.test");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert!(org.is_none(), "apex must NEVER resolve to a tenant");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn subdomain_resolver_unknown_subdomain_returns_none() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = SubdomainResolver::new("app.test");
    let parts = parts_with_host("ghost.app.test");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert!(org.is_none(), "unknown subdomain → no match");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn subdomain_resolver_strips_port_from_host_header() {
    // `Host: acme.app.test:8080` should still match host_pattern
    // `acme.app.test`. Common in dev where you bind a non-standard port.
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = SubdomainResolver::new("app.test");
    let parts = parts_with_host("acme.app.test:8080");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert_eq!(org.unwrap().slug, "acme");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn subdomain_resolver_skips_inactive_orgs() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = SubdomainResolver::new("app.test");
    // `frozen` has host_pattern set but active=false.
    let parts = parts_with_host("frozen.app.test");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert!(org.is_none(), "inactive orgs must not resolve");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn path_prefix_resolver_matches_first_segment() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = PathPrefixResolver;
    let parts = parts_with_uri("http://app.test/acme/dashboard");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert_eq!(org.unwrap().slug, "acme");

    let parts = parts_with_uri("http://app.test/");
    assert!(r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap()
        .is_none());

    let parts = parts_with_uri("http://app.test/unknown/x");
    assert!(r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap()
        .is_none());

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn header_resolver_matches_x_org() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = HeaderResolver::default();
    let parts = parts_with_header("x-org", "globex");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert_eq!(org.unwrap().slug, "globex");

    // Unknown slug.
    let parts = parts_with_header("x-org", "ghost");
    assert!(r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap()
        .is_none());

    // Empty value.
    let parts = parts_with_header("x-org", "");
    assert!(r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap()
        .is_none());

    // No header at all.
    let parts = parts_with_uri("http://app.test/");
    assert!(r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap()
        .is_none());

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn port_resolver_matches_org_port() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let r = PortResolver;
    let parts = parts_with_uri("http://example.test:9001/x");
    let org = r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap();
    assert_eq!(org.unwrap().slug, "initech");

    let parts = parts_with_uri("http://example.test:8080/x");
    assert!(r
        .resolve(&parts, &rustango::sql::Pool::Postgres(pool.clone()))
        .await
        .unwrap()
        .is_none());

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn chain_resolver_subdomain_first_then_header() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let chain = ChainResolver::standard("app.test");

    // Subdomain wins when both are present.
    let mut req = Request::builder()
        .uri("http://acme.app.test/")
        .body(())
        .unwrap();
    req.headers_mut().insert(
        http::header::HOST,
        HeaderValue::from_static("acme.app.test"),
    );
    req.headers_mut().insert(
        HeaderName::from_static("x-org"),
        HeaderValue::from_static("globex"),
    );
    let (parts, _) = req.into_parts();
    let org = chain.resolve(&parts, &pool).await.unwrap();
    assert_eq!(
        org.unwrap().slug,
        "acme",
        "subdomain should win over X-Org in the standard chain"
    );

    // Header fallback when no subdomain matches.
    let parts = parts_with_header("x-org", "globex");
    let org = chain.resolve(&parts, &pool).await.unwrap();
    assert_eq!(org.unwrap().slug, "globex");

    // Path-prefix is NOT in the standard chain — `/acme/x` alone
    // shouldn't resolve.
    let parts = parts_with_uri("http://app.test/acme/x");
    assert!(
        chain.resolve(&parts, &pool).await.unwrap().is_none(),
        "PathPrefixResolver must NOT be in the default chain"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn chain_resolver_with_explicit_path_prefix_opt_in() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let chain = ChainResolver::new()
        .push(SubdomainResolver::new("app.test"))
        .push(PathPrefixResolver);

    // No subdomain match → falls through to path-prefix.
    let parts = parts_with_uri("http://app.test/acme/dashboard");
    let org = chain.resolve(&parts, &pool).await.unwrap();
    assert_eq!(org.unwrap().slug, "acme");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn chain_resolver_returns_none_when_no_resolver_matches() {
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    seed_orgs(&pool).await;

    let chain = ChainResolver::standard("app.test");
    let parts = parts_with_uri("http://app.test/");
    assert!(chain.resolve(&parts, &pool).await.unwrap().is_none());

    migrate::drop_all(&pool).await.unwrap();
}
