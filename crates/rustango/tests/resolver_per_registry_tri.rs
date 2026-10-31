//! Two registries in one process keep their own resolver caches and
//! breaker (#2077). Registry A is a SQLite file; B is the backend under test.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "testkit"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds.

use http::request::Parts;
use http::Request;
use rustango::sql::CounterPool as _;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{
    invalidate_org_cache, HeaderResolver, ListenerPort, Org, OrgResolver, PathPrefixResolver,
    PortResolver, SubdomainResolver,
};
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

/// `Host: acme.app.test.` is the same host as `acme.app.test`.
async fn a_trailing_dot_host_resolves_the_tenant(pool: &Pool) {
    let slug = unique("d");
    let host = add_org(pool, &slug).await;
    assert_eq!(
        resolve(pool, &format!("{host}.:8080")).await.unwrap(),
        Some(slug)
    );
}

/// A port no tenant in the shared registry holds yet.
async fn free_port(pool: &Pool) -> i32 {
    let mut p = 20_000 + i32::try_from(uuid::Uuid::new_v4().as_u128() % 30_000).unwrap();
    while Org::objects().filter("port", p).count(pool).await.unwrap() > 0 {
        p += 1;
    }
    p
}

/// What each resolver answers for one tenant: base host, apex + path
/// prefix, `X-Org` header and listener port.
async fn answers(pool: &Pool, slug: &str, port: i32) -> [Option<String>; 4] {
    let host = Request::builder()
        .uri("/")
        .header("host", format!("{slug}.app.test"))
        .body(())
        .unwrap()
        .into_parts()
        .0;
    let prefix = Request::builder()
        .uri(format!("/{slug}/page"))
        .header("host", "app.test")
        .body(())
        .unwrap()
        .into_parts()
        .0;
    let header = Request::builder()
        .uri("/")
        .header("x-org", slug)
        .body(())
        .unwrap()
        .into_parts()
        .0;
    let mut by_port = parts_for_host("app.test");
    by_port
        .extensions
        .insert(ListenerPort(u16::try_from(port).unwrap()));
    let slug_of = |r: Result<Option<Org>, _>| r.expect("resolve").map(|o: Org| o.slug);
    [
        slug_of(
            SubdomainResolver::new("app.test")
                .resolve(&host, pool)
                .await,
        ),
        slug_of(PathPrefixResolver.resolve(&prefix, pool).await),
        slug_of(HeaderResolver::default().resolve(&header, pool).await),
        slug_of(PortResolver.resolve(&by_port, pool).await),
    ]
}

/// Header and port lookups are cached like the host and prefix ones,
/// misses included, and dropped by the same invalidation.
async fn header_and_port_lookups_are_cached(pool: &Pool) {
    // Only the explicit invalidation may drop the cache here.
    std::env::set_var("RUSTANGO_HOST_GEN_CHECK_SECS", "0");
    let slug = unique("c");
    let port = free_port(pool).await;
    assert_eq!(answers(pool, &slug, port).await, [None, None, None, None]);

    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some(format!("{slug}.app.test")),
        path_prefix: Some(format!("/{slug}")),
        port: Some(port),
        ..rustango::testkit::org()
    };
    org.insert_pool(pool).await.expect("insert org");
    assert_eq!(
        answers(pool, &slug, port).await,
        [None, None, None, None],
        "a cached miss was not served"
    );

    invalidate_org_cache();
    let all = Some(slug.clone());
    assert_eq!(
        answers(pool, &slug, port).await,
        [all.clone(), all.clone(), all.clone(), all.clone()]
    );

    // Suspended: hits are served until the invalidation, then nothing resolves.
    org.active = false;
    org.save_pool(pool).await.expect("suspend");
    assert_eq!(
        answers(pool, &slug, port).await,
        [all.clone(), all.clone(), all.clone(), all]
    );
    invalidate_org_cache();
    assert_eq!(answers(pool, &slug, port).await, [None, None, None, None]);
    Org::objects()
        .filter("slug", slug.as_str())
        .delete_on(pool)
        .await
        .expect("drop org");
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        a_trailing_dot_host_resolves_the_tenant,
        cached_orgs_stay_with_their_registry,
        a_broken_registry_does_not_trip_another,
        header_and_port_lookups_are_cached,
    ],
}
