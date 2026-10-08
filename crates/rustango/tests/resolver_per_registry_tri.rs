//! Two registries in one process keep their own resolver caches and
//! breaker (#2077). Registry A is a SQLite file; B is the backend under test.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "testkit"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds.

use http::request::Parts;
use http::Request;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{Org, OrgResolver, SubdomainResolver};
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn parts_for_host(host: &str) -> Parts {
    Request::builder()
        .uri("/")
        .header("host", host)
        .body(())
        .expect("request")
        .into_parts()
        .0
}

fn unique(label: &str) -> String {
    format!("{label}{}", uuid::Uuid::new_v4().simple())
}

/// Insert an active org whose base host is `<slug>.app.test`.
async fn add_org(pool: &Pool, slug: &str) -> String {
    let host = format!("{slug}.app.test");
    let mut org = Org {
        slug: slug.into(),
        display_name: slug.into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(pool).await.expect("insert org");
    host
}

async fn resolve(pool: &Pool, host: &str) -> Result<Option<String>, String> {
    SubdomainResolver::new("app.test")
        .resolve(&parts_for_host(host), pool)
        .await
        .map(|o| o.map(|o| o.slug))
        .map_err(|e| e.to_string())
}

/// An org cached through A must not answer for B, which has no such row.
async fn cached_orgs_stay_with_their_registry(pool: &Pool) {
    let a = rustango::testkit::matrix::sqlite_file_pool().await;
    setup(&a).await;
    let slug = unique("a");
    let host = add_org(&a, &slug).await;

    assert_eq!(resolve(&a, &host).await.unwrap(), Some(slug));
    assert_eq!(
        resolve(pool, &host).await.unwrap(),
        None,
        "B answered from A's cache"
    );
}

/// A's open breaker must not fail lookups on a healthy B.
async fn a_broken_registry_does_not_trip_another(pool: &Pool) {
    let a = rustango::testkit::matrix::sqlite_file_pool().await;
    setup(&a).await;
    let Pool::Sqlite(sq) = &a else {
        unreachable!("A is SQLite")
    };
    sqlx::query("DROP TABLE rustango_orgs")
        .execute(sq)
        .await
        .expect("break A");
    assert!(resolve(&a, &unique("x.app.test")).await.is_err());

    let slug = unique("b");
    let host = add_org(pool, &slug).await;
    assert_eq!(
        resolve(pool, &host).await.expect("B failed on A's breaker"),
        Some(slug)
    );
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        cached_orgs_stay_with_their_registry,
        a_broken_registry_does_not_trip_another,
    ],
}
