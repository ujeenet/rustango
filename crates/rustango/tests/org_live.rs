//! Live tests for the `Org` registry model.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently — same
//! convention as the rest of the workspace's live suites.
//!
//! Slice 1 scope: insert + fetch + filter-by-slug round-trip. Resolver,
//! TenantPools, and scoped migrations land in slices 2-3.

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::migrate;
use rustango::tenancy::{Org, StorageMode};

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

#[tokio::test]
async fn org_round_trip_insert_and_fetch() {
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut acme = Org {
        id: Auto::default(),
        slug: "acme".into(),
        display_name: "ACME Corp".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some("acme".into()),
        host_pattern: Some("acme.app.test".into()),
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
    };
    acme.insert(&pool).await.unwrap();
    let acme_id = *acme.id.get().unwrap();
    assert!(acme_id > 0, "BIGSERIAL should assign a positive id");

    let fetched: Vec<Org> = Org::objects().fetch(&pool).await.unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].slug, "acme");
    assert_eq!(fetched[0].storage_mode, "schema");
    assert_eq!(fetched[0].host_pattern.as_deref(), Some("acme.app.test"));

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn org_filter_by_slug_returns_single_match() {
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    for slug in ["acme", "globex", "initech"] {
        let mut org = Org {
            id: Auto::default(),
            slug: slug.into(),
            display_name: slug.to_uppercase(),
            storage_mode: StorageMode::Database.as_str().into(),
            database_url: Some(format!("env://TENANT_{}_DB_URL", slug.to_uppercase())),
            schema_name: None,
            host_pattern: Some(format!("{slug}.app.test")),
            port: None,
            path_prefix: None,
            active: true,
            created_at: now(),
        };
        org.insert(&pool).await.unwrap();
    }

    let matches: Vec<Org> = Org::objects()
        .where_(Org::slug.eq("globex"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].slug, "globex");
    assert_eq!(matches[0].storage_mode, "database");
    assert_eq!(
        matches[0].database_url.as_deref(),
        Some("env://TENANT_GLOBEX_DB_URL"),
        "secret-reference round-trips verbatim — Slice 3.5 will resolve it"
    );

    let actives: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(actives.len(), 3);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn org_inactive_orgs_are_persistable_and_queryable() {
    // Soft-disable: `active = false` keeps the row but the eventual
    // resolver will reject. Slice 1 just confirms the column persists.
    let Some(pool) = pool().await else {
        return;
    };

    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let mut frozen = Org {
        id: Auto::default(),
        slug: "frozen".into(),
        display_name: "Frozen Tenant".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some("frozen".into()),
        host_pattern: Some("frozen.app.test".into()),
        port: None,
        path_prefix: None,
        active: false,
        created_at: now(),
    };
    frozen.insert(&pool).await.unwrap();

    let fetched: Vec<Org> = Org::objects()
        .where_(Org::active.eq(false))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(fetched.len(), 1);
    assert!(!fetched[0].active);

    migrate::drop_all(&pool).await.unwrap();
}

#[test]
fn storage_mode_string_round_trip() {
    assert_eq!(StorageMode::Schema.as_str(), "schema");
    assert_eq!(StorageMode::Database.as_str(), "database");
    assert_eq!(StorageMode::parse("schema").unwrap(), StorageMode::Schema);
    assert_eq!(
        StorageMode::parse("database").unwrap(),
        StorageMode::Database
    );
    assert!(StorageMode::parse("nope").is_err());
}
