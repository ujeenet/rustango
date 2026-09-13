//! Base-host resolution is cached, and the cache is safe.
//!
//! `SubdomainResolver` runs first in the standard chain and matches the
//! base `Org.host_pattern`, so it is on the path of every request.
//! Uncached it cost exactly one registry `SELECT` per request — measured
//! against a live Postgres with `log_statement=all`:
//!
//! ```text
//!                        q/req before   q/req after
//! base host                      1.00          0.00
//! registered extra host          2.00          1.00
//! unknown host                   1.00          0.00
//! ```
//!
//! That query was the registry's hot-path SPOF: it is why an unreachable
//! registry took down tenants whose own databases were perfectly fine.
//!
//! The risk a cache introduces is staleness, so most of what follows
//! pins the *invalidation*, not the hit.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "testkit"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds.

use http::request::Parts;
use http::Request;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{Org, OrgResolver, SubdomainResolver};

/// Both the cache and its fingerprint are process-global.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
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

/// File-backed so rows can be changed out from under the pool.
async fn registry() -> (Pool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pool = Pool::connect(&url).await.expect("sqlite");
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework tables");
    rustango::testkit::reset_org_cache();
    (pool, dir)
}

async fn add_org(pool: &Pool, slug: &str, host: &str) {
    let mut org = Org {
        id: rustango::sql::Auto::Unset,
        slug: slug.into(),
        display_name: slug.into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some(host.into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(pool).await.expect("insert org");
}

async fn resolve(pool: &Pool, host: &str) -> Option<String> {
    SubdomainResolver::new("app.test")
        .resolve(&parts_for_host(host), pool)
        .await
        .expect("resolve")
        .map(|o| o.slug)
}

/// The hit must not touch the database at all.
///
/// Asserted by deleting the row behind the cache's back: a resolver that
/// queried would find nothing and return `None`, so still getting the
/// tenant is positive proof no query was issued. That is stronger than
/// timing, which would be flaky, and than counting queries, which SQLite
/// will not report.
#[tokio::test]
async fn a_cached_base_host_is_served_without_querying() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "acme.app.test").await;
    rustango::testkit::reset_org_cache();

    assert_eq!(
        resolve(&pool, "acme.app.test").await.as_deref(),
        Some("acme")
    );

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query("DELETE FROM rustango_orgs")
        .execute(sq)
        .await
        .expect("delete");

    assert_eq!(
        resolve(&pool, "acme.app.test").await.as_deref(),
        Some("acme"),
        "the row is gone; still resolving proves the answer came from cache"
    );
}

/// The negative entry matters more than the positive one: this resolver
/// runs first for *every* request, so an unregistered host would be a
/// free registry query per request — the amplification a sprayed `Host`
/// header buys. Same proof, inverted: insert the row behind the cache
/// and confirm it is still a miss.
#[tokio::test]
async fn an_unknown_host_is_negatively_cached() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    rustango::testkit::reset_org_cache();

    assert!(resolve(&pool, "nobody.app.test").await.is_none());

    add_org(&pool, "late", "nobody.app.test").await;
    rustango::testkit::reset_registry_breaker();

    assert!(
        resolve(&pool, "nobody.app.test").await.is_none(),
        "the row exists now; still missing proves the miss was cached"
    );

    // …and clearing the cache (as the TTL or a fingerprint bump would)
    // lets it through.
    rustango::testkit::reset_org_cache();
    assert_eq!(
        resolve(&pool, "nobody.app.test").await.as_deref(),
        Some("late")
    );
}

/// Suspension is the sharpest staleness risk — `manage drop-tenant`
/// soft-deletes with `active = false`, and a suspended tenant must stop
/// serving. `active` is a term in the fingerprint precisely so this
/// converges across pods on the poll interval rather than the TTL.
#[tokio::test]
async fn suspending_a_tenant_moves_the_fingerprint() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "acme.app.test").await;

    let before = rustango::tenancy::org_generation(&pool)
        .await
        .expect("generation");

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'acme'")
        .execute(sq)
        .await
        .expect("suspend");

    let after = rustango::tenancy::org_generation(&pool)
        .await
        .expect("generation");

    assert_eq!(
        (after.count, after.max_id),
        (before.count, before.max_id),
        "precondition: a suspend moves neither count nor max_id — that is \
         why the active terms exist"
    );
    assert_ne!(
        after, before,
        "a suspend must move the fingerprint, or other pods keep serving \
         a tenant that has been shut off"
    );
}

/// Two opposite toggles in one interval must not cancel: suspend one
/// tenant and reactivate another and the *count* of active rows is
/// unchanged. `active_id_sum` is what distinguishes which are live.
#[tokio::test]
async fn compensating_suspends_still_move_the_fingerprint() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "aaa", "aaa.app.test").await;
    add_org(&pool, "bbb", "bbb.app.test").await;

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'bbb'")
        .execute(sq)
        .await
        .expect("suspend bbb");

    let before = rustango::tenancy::org_generation(&pool)
        .await
        .expect("generation");

    // The swap: aaa off, bbb on.
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'aaa'")
        .execute(sq)
        .await
        .expect("suspend aaa");
    sqlx::query("UPDATE rustango_orgs SET active = 1 WHERE slug = 'bbb'")
        .execute(sq)
        .await
        .expect("reactivate bbb");

    let after = rustango::tenancy::org_generation(&pool)
        .await
        .expect("generation");

    assert_eq!(
        (after.count, after.active, after.max_id),
        (before.count, before.active, before.max_id),
        "precondition: count / active / max_id are all unchanged by the swap"
    );
    assert_ne!(after, before, "active_id_sum must catch the swap");
}

/// An inactive org must never resolve, cached or not — the cache stores
/// what the query returned, and the query filters on `active`.
#[tokio::test]
async fn an_inactive_org_never_resolves() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "acme.app.test").await;
    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'acme'")
        .execute(sq)
        .await
        .expect("suspend");
    rustango::testkit::reset_org_cache();

    assert!(
        resolve(&pool, "acme.app.test").await.is_none(),
        "a suspended tenant must not resolve"
    );
}

/// A registered *extra* host is served from cache too — and because
/// that cache now holds the `Org` rather than its id, a hit costs no
/// query at all.
///
/// Same proof as the base-host case: delete the tenant behind the
/// cache's back and confirm the host still resolves.
#[tokio::test]
async fn a_cached_extra_host_is_served_without_querying() {
    use rustango::tenancy::RegisteredHostResolver;
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "acme.app.test").await;
    rustango::tenancy::add_host(&pool, "acme", "extra.example.test")
        .await
        .expect("add_host");
    rustango::testkit::reset_org_cache();
    rustango::tenancy::invalidate_host_cache();

    let first = RegisteredHostResolver
        .resolve(&parts_for_host("extra.example.test"), &pool)
        .await
        .expect("resolve");
    assert_eq!(first.map(|o| o.slug).as_deref(), Some("acme"));

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    // Suspend rather than delete: `rustango_org_hosts` has a foreign key
    // to the org, so the row cannot be removed while a host points at
    // it. Suspending works just as well as proof — the lookup filters on
    // `active`, so a resolver that queried would return `None`.
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'acme'")
        .execute(sq)
        .await
        .expect("suspend org");

    let second = RegisteredHostResolver
        .resolve(&parts_for_host("extra.example.test"), &pool)
        .await
        .expect("resolve");
    assert_eq!(
        second.map(|o| o.slug).as_deref(),
        Some("acme"),
        "the org is suspended; still resolving proves the hit needed no \
         query — caching the id instead would have cost one here, and \
         returned None"
    );
}

/// Because `HOST_CACHE` now holds `Org` rows, a change to
/// `rustango_orgs` has to invalidate it as well — an extra hostname
/// pointing at a tenant that has since been suspended must stop
/// resolving on the same bound as that tenant's base host.
#[tokio::test]
async fn suspending_a_tenant_also_clears_the_extra_host_cache() {
    use rustango::tenancy::RegisteredHostResolver;
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "acme.app.test").await;
    rustango::tenancy::add_host(&pool, "acme", "extra2.example.test")
        .await
        .expect("add_host");
    rustango::testkit::reset_org_cache();
    rustango::tenancy::invalidate_host_cache();

    assert!(RegisteredHostResolver
        .resolve(&parts_for_host("extra2.example.test"), &pool)
        .await
        .expect("resolve")
        .is_some());

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'acme'")
        .execute(sq)
        .await
        .expect("suspend");
    // ONLY the org-level invalidation — exactly what `drop-tenant` and
    // the operator console call. Calling `invalidate_host_cache()` here
    // as well would make this pass no matter what
    // `invalidate_org_cache` does, which is how an earlier version of
    // this test managed to cover nothing.
    rustango::tenancy::invalidate_org_cache();

    assert!(
        RegisteredHostResolver
            .resolve(&parts_for_host("extra2.example.test"), &pool)
            .await
            .expect("resolve")
            .is_none(),
        "a suspended tenant must stop answering on its extra hosts too — \
         `invalidate_org_cache` has to reach HOST_CACHE, which now holds \
         Org rows"
    );
}

/// The `Host` header is client-supplied and hostnames are
/// case-insensitive (RFC 4343), so case variants must not become
/// distinct cache keys. Otherwise a few thousand spellings of one real
/// host fill a bounded map and evict every genuine tenant.
#[tokio::test]
async fn host_case_variants_share_one_cache_entry() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "acme.app.test").await;
    rustango::testkit::reset_org_cache();

    assert_eq!(
        resolve(&pool, "acme.app.test").await.as_deref(),
        Some("acme")
    );

    // Suspend behind the cache: a second key would miss the cache, query,
    // and return None. Sharing the entry returns the cached tenant.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query("UPDATE rustango_orgs SET active = 0 WHERE slug = 'acme'")
        .execute(sq)
        .await
        .expect("suspend");

    for variant in ["ACME.APP.TEST", "AcMe.App.Test", "acme.app.test"] {
        assert_eq!(
            resolve(&pool, variant).await.as_deref(),
            Some("acme"),
            "`{variant}` must hit the same entry as the lowercase host"
        );
    }
}

/// The apex is not a tenant and must short-circuit before the cache, so
/// it can never occupy an entry or be served one.
#[tokio::test]
async fn the_apex_is_never_cached_as_a_tenant() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    add_org(&pool, "acme", "app.test").await; // deliberately the apex
    rustango::testkit::reset_org_cache();

    assert!(
        resolve(&pool, "app.test").await.is_none(),
        "the apex hosts the operator console, never a tenant"
    );
}
