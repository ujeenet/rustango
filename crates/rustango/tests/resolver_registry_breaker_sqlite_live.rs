//! Tenant resolution fails fast while the registry is unreachable.
//!
//! Resolution runs before everything else and is uncached, so an
//! unreachable registry is paid on *every* request — each one waiting
//! out the pool's acquire timeout before erroring. That pins a worker
//! for the whole timeout, so the server saturates and every tenant goes
//! down, including tenants whose own databases are perfectly healthy.
//!
//! Measured against a stopped Postgres registry, two pods behind the
//! framework's own `tenancy::server::run` stack:
//!
//! ```text
//! registry UP     303   0.002s
//! registry DOWN   500  30.004s   <- every request, sqlx's default
//! registry DOWN   500  30.005s      acquire_timeout
//! registry BACK   303   0.035s
//! ```
//!
//! Two changes bound that: `Pool::connect` now sets an explicit
//! acquire timeout instead of inheriting sqlx's 30s, and the resolver
//! records a failure so the requests behind the first one return
//! immediately rather than queueing for the same doomed connection.
//!
//! Note the breaker is deliberately **not** a behaviour change — the
//! caller still gets the same `Err`, just without the wait.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "testkit"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds.

use http::request::Parts;
use http::Request;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{OrgResolver, SubdomainResolver};

/// Resolution state is process-global, so these tests cannot share a
/// process with each other unserialised.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn parts_for_host(host: &str) -> Parts {
    let req = Request::builder()
        .uri("/")
        .header("host", host)
        .body(())
        .expect("request");
    req.into_parts().0
}

/// A file-backed registry (not `:memory:`) so the table can be renamed
/// out from under the pool and put back.
async fn registry() -> (Pool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pool = Pool::connect(&url).await.expect("sqlite");
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework tables");
    let mut org = rustango::tenancy::Org {
        id: rustango::sql::Auto::Unset,
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some("acme.app.test".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&pool).await.expect("insert org");
    (pool, dir)
}

/// The headline: once a lookup has failed, the next one must not go to
/// the database at all.
///
/// Asserted without timing, which would be flaky, and without counting
/// queries, which SQLite will not report. Instead the registry is
/// repaired *before* the second call: a resolver that queried would
/// find the org and return it, so an `Err` is positive proof that no
/// query was issued.
#[tokio::test]
async fn a_failed_registry_lookup_short_circuits_the_next_one() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    rustango::testkit::reset_registry_breaker();

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };

    // Healthy to begin with.
    let hit = SubdomainResolver::new("app.test")
        .resolve(&parts_for_host("acme.app.test"), &pool)
        .await
        .expect("healthy registry resolves");
    assert_eq!(hit.map(|o| o.slug), Some("acme".to_owned()));

    // Break the registry and resolve — errors, and arms the breaker.
    sqlx::query("ALTER TABLE rustango_orgs RENAME TO orgs_hidden")
        .execute(sq)
        .await
        .expect("rename away");
    rustango::testkit::reset_registry_breaker();
    // Clear the base-host cache too, or the resolver answers this host
    // from memory and never reaches the query under test. Worth noting
    // that behaviour is a *feature* of the cache — a host already seen
    // keeps resolving straight through a registry outage — it just has
    // to be stepped around to exercise the breaker.
    rustango::testkit::reset_org_cache();
    let broken = SubdomainResolver::new("app.test")
        .resolve(&parts_for_host("acme.app.test"), &pool)
        .await;
    assert!(broken.is_err(), "an unreachable registry must surface Err");

    // Repair it. The row is present and resolvable again.
    sqlx::query("ALTER TABLE orgs_hidden RENAME TO rustango_orgs")
        .execute(sq)
        .await
        .expect("rename back");

    let still_open = SubdomainResolver::new("app.test")
        .resolve(&parts_for_host("acme.app.test"), &pool)
        .await;
    assert!(
        still_open.is_err(),
        "the breaker must still be open — returning the org here would \
         mean the resolver queried, which is the per-request stall this \
         exists to prevent"
    );

    // Clearing it (as the retry window elapsing would) restores service.
    rustango::testkit::reset_registry_breaker();
    let recovered = SubdomainResolver::new("app.test")
        .resolve(&parts_for_host("acme.app.test"), &pool)
        .await
        .expect("resolve");
    assert_eq!(
        recovered.map(|o| o.slug),
        Some("acme".to_owned()),
        "once the window lapses the resolver must query again and succeed"
    );
}

/// A healthy registry must never be short-circuited, however many
/// requests it serves — the breaker only closes on an actual failure.
#[tokio::test]
async fn a_healthy_registry_is_never_short_circuited() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    rustango::testkit::reset_registry_breaker();

    for i in 0..25 {
        let got = SubdomainResolver::new("app.test")
            .resolve(&parts_for_host("acme.app.test"), &pool)
            .await
            .unwrap_or_else(|e| panic!("request {i} must resolve, got {e:?}"));
        assert_eq!(got.map(|o| o.slug), Some("acme".to_owned()));
    }

    // And a genuine miss stays a miss (`Ok(None)`), not an error — a
    // resolver that conflated the two would make every unknown host
    // look like an outage and arm the breaker.
    let miss = SubdomainResolver::new("app.test")
        .resolve(&parts_for_host("nobody.app.test"), &pool)
        .await
        .expect("a miss is not an error");
    assert!(miss.is_none());

    let after_miss = SubdomainResolver::new("app.test")
        .resolve(&parts_for_host("acme.app.test"), &pool)
        .await
        .expect("a miss must not have armed the breaker");
    assert_eq!(after_miss.map(|o| o.slug), Some("acme".to_owned()));
}

/// `Pool::connect` must not leave sqlx's 30s default in place.
#[tokio::test]
async fn pool_connect_sets_a_bounded_acquire_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
    let pool = Pool::connect(&url).await.expect("sqlite");
    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };
    // sqlx exposes the configured value; 30s would mean the default
    // leaked through and a dead database still pins a worker for half
    // a minute per request.
    assert!(
        sq.options().get_acquire_timeout() < std::time::Duration::from_secs(30),
        "Pool::connect must set an explicit acquire timeout, got {:?}",
        sq.options().get_acquire_timeout()
    );
}
