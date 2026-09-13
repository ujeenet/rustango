//! Extra tenant hostnames (`rustango_org_hosts`) against a real registry.
//!
//! The rules worth pinning are the ones a unit test cannot reach: a host
//! may be bound and unbound freely, but the tenant's **base** host — the
//! `Org.host_pattern` written by `create-tenant` — must survive every
//! removal path, because a tenant with no host is unreachable.
//!
//! This file is the engine. The console surface that drives it lives in
//! `operator_console_hosts_sqlite_live`.
//! NOTE on isolation: the resolver's cache is process-global and keyed by
//! hostname, so each test below uses a hostname of its own. Production has
//! one registry per process, which is why the cache needs no registry key.
#![cfg(all(feature = "tenancy", feature = "sqlite"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds.

use rustango::core::Column as _;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{
    add_host, list_for_org, normalize_hostname, remove_host, set_host_enabled, HostError, Org,
};

/// Suite-wide lock. The resolver's cache is process-global, so a test that
/// asserts on cache behaviour cannot run beside one whose `add_host` clears
/// it — that is not a hypothetical: without this, dropping the invalidation
/// hook entirely still left the invalidation test green, because a sibling
/// test happened to clear the cache in between.
fn cache_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// A registry on an in-memory SQLite DB with one org whose base host is
/// `acme.example.com`.
///
/// Resets **both** pieces of resolver state, not just the resolution
/// cache. The host-table fingerprint is process-global and keyed by
/// nothing, so it cannot be side-stepped by giving each test its own
/// hostnames the way `HOST_CACHE` can: a fingerprint left over from the
/// previous test's registry gets compared against this brand-new one, and
/// the mismatch fires a spurious invalidation partway through whichever
/// test runs next. It only shows up when more than the poll interval
/// elapses between two tests — a debug CI runner, `--test-threads=1`, a
/// slow `migrate_framework` — which is the worst kind of flake to chase.
async fn registry_with_org() -> Pool {
    rustango::testkit::reset_host_generation();
    // The resolver also fails fast for a window after a registry error.
    // Nothing here arms it today, but it is the same class of
    // process-global leak as the fingerprint above, and leaving it to
    // chance is how the fingerprint one got found in the first place.
    rustango::testkit::reset_registry_breaker();
    // `SubdomainResolver` caches hostname -> Org now, and these tests
    // stand up a fresh registry each time — a leftover entry would be
    // answered from the previous test's data.
    rustango::testkit::reset_org_cache();
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    );
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework tables");
    let mut org = Org {
        id: rustango::sql::Auto::Unset,
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some("acme.example.com".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&pool).await.expect("insert org");
    pool
}

#[tokio::test]
async fn base_host_is_listed_and_flagged() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    let hosts = list_for_org(&pool, "acme").await.expect("list");
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].hostname, "acme.example.com");
    assert!(hosts[0].is_base, "the org's host_pattern must be flagged");
    assert!(hosts[0].id.is_none(), "the base host has no row of its own");
}

#[tokio::test]
async fn an_extra_host_can_be_added_listed_and_removed() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    add_host(&pool, "acme", "add-remove.acme.com")
        .await
        .expect("add");

    let hosts = list_for_org(&pool, "acme").await.expect("list");
    assert_eq!(hosts.len(), 2);
    assert!(hosts[0].is_base, "base sorts first");
    let extra = &hosts[1];
    assert_eq!(extra.hostname, "add-remove.acme.com");
    assert!(!extra.is_base);
    assert!(extra.id.is_some(), "an extra host is a real row");

    remove_host(&pool, "acme", "add-remove.acme.com")
        .await
        .expect("remove");
    let hosts = list_for_org(&pool, "acme").await.expect("list");
    assert_eq!(hosts.len(), 1, "only the base host remains");
    assert!(hosts[0].is_base);
}

/// The rule the whole design is arranged around.
#[tokio::test]
async fn the_base_host_cannot_be_removed() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    let err = remove_host(&pool, "acme", "acme.example.com")
        .await
        .expect_err("removing the base host must fail");
    assert!(
        matches!(err, HostError::IsBaseHost(_)),
        "expected IsBaseHost, got {err:?}"
    );
    // And it is still there.
    let hosts = list_for_org(&pool, "acme").await.expect("list");
    assert!(hosts.iter().any(|h| h.is_base));
}

#[tokio::test]
async fn the_base_host_cannot_be_disabled_either() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    // Disabling is removal by another name — an unreachable tenant either
    // way — so it has to be refused on the same grounds.
    let err = set_host_enabled(&pool, "acme", "acme.example.com", false)
        .await
        .expect_err("disabling the base host must fail");
    assert!(matches!(err, HostError::IsBaseHost(_)), "got {err:?}");
}

#[tokio::test]
async fn a_host_cannot_be_claimed_twice() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    add_host(&pool, "acme", "twice.acme.com")
        .await
        .expect("add");
    let err = add_host(&pool, "acme", "twice.acme.com")
        .await
        .expect_err("duplicate must fail");
    assert!(matches!(err, HostError::Taken(_)), "got {err:?}");
}

/// A registered host that duplicates some tenant's base would insert fine
/// and then never resolve, because `SubdomainResolver` runs first. Refusing
/// it up front turns a silent routing puzzle into an error message.
#[tokio::test]
async fn a_host_matching_another_tenants_base_is_refused() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    let err = add_host(&pool, "acme", "acme.example.com")
        .await
        .expect_err("must not shadow a base host");
    assert!(matches!(err, HostError::Taken(_)), "got {err:?}");
}

#[tokio::test]
async fn removal_is_scoped_to_the_owning_org() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    let mut other = Org {
        id: rustango::sql::Auto::Unset,
        slug: "globex".into(),
        display_name: "Globex".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some("globex.example.com".into()),
        ..rustango::testkit::org()
    };
    other.insert_pool(&pool).await.expect("insert org");
    add_host(&pool, "acme", "scoped.acme.com")
        .await
        .expect("add");

    // Globex must not be able to unbind Acme's host by naming it.
    let err = remove_host(&pool, "globex", "scoped.acme.com")
        .await
        .expect_err("cross-tenant removal must fail");
    assert!(matches!(err, HostError::NotFound), "got {err:?}");
    assert_eq!(list_for_org(&pool, "acme").await.expect("list").len(), 2);
}

#[tokio::test]
async fn an_extra_host_can_be_disabled_without_unbinding_it() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    add_host(&pool, "acme", "disabled.acme.com")
        .await
        .expect("add");
    set_host_enabled(&pool, "acme", "disabled.acme.com", false)
        .await
        .expect("disable");
    let hosts = list_for_org(&pool, "acme").await.expect("list");
    let extra = hosts.iter().find(|h| !h.is_base).expect("still bound");
    assert!(!extra.enabled, "disabled but retained");
}

#[tokio::test]
async fn hostnames_are_normalized_on_the_way_in() {
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    add_host(&pool, "acme", "  SHOP.Acme.COM  ")
        .await
        .expect("add");
    let hosts = list_for_org(&pool, "acme").await.expect("list");
    assert!(hosts.iter().any(|h| h.hostname == "shop.acme.com"));
    // ...so the same host in a different case is a duplicate, not a second row.
    assert!(add_host(&pool, "acme", "Shop.Acme.Com").await.is_err());
    assert_eq!(
        normalize_hostname("Shop.Acme.Com").unwrap(),
        "shop.acme.com"
    );
}

/// The upgrade path, which is the whole backward-compatibility question:
/// an existing registry that predates this feature must gain
/// `rustango_org_hosts` from the ordinary `migrate` an operator already
/// runs, with nothing hand-written and nothing else disturbed.
#[tokio::test]
async fn an_existing_registry_gains_the_table_on_a_plain_migrate() {
    let _guard = cache_lock().lock().await;
    use rustango::sql::FetcherPool as _;

    let dir = tempdir_unique("org-hosts-upgrade");
    std::fs::create_dir_all(dir.join("migrations")).expect("migrations dir");
    let db = dir.join("registry.db");
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect(&format!("sqlite:{}?mode=rwc", db.display()))
            .await
            .expect("sqlite"),
    );

    // A pre-upgrade registry: every framework table EXCEPT the new one.
    let models: Vec<&'static rustango::core::ModelSchema> = rustango::migrate::registered_models()
        .into_iter()
        .filter(|m| {
            m.managed && m.table.starts_with("rustango_") && m.table != "rustango_org_hosts"
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    let models: Vec<_> = models
        .into_iter()
        .filter(|m| seen.insert(m.table))
        .collect();
    rustango::testkit::create_tables(&pool, &models)
        .await
        .expect("pre-upgrade tables");
    let mut org = Org {
        id: rustango::sql::Auto::Unset,
        slug: "legacy".into(),
        display_name: "Legacy".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some("legacy.example.com".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&pool).await.expect("existing org survives");

    assert!(
        !table_exists(&pool, "rustango_org_hosts").await,
        "precondition: the table must be absent before the upgrade"
    );

    // Exactly what an operator runs after upgrading the binary.
    rustango::tenancy::migrate_registry_pool(&pool, &dir.join("migrations"))
        .await
        .expect("migrate must succeed against a pre-upgrade registry");

    assert!(
        table_exists(&pool, "rustango_org_hosts").await,
        "the generated system chain must create the new table"
    );
    // And the pre-existing data is untouched.
    let orgs: Vec<Org> = Org::objects().fetch(&pool).await.expect("orgs");
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].slug, "legacy");
    assert_eq!(orgs[0].host_pattern.as_deref(), Some("legacy.example.com"));

    // The feature is live immediately after that migrate.
    add_host(&pool, "legacy", "extra.example.com")
        .await
        .expect("add after upgrade");
    let hosts = list_for_org(&pool, "legacy").await.expect("list");
    assert_eq!(hosts.len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Re-migrating a registry whose `rustango_org_hosts` **already has rows**
/// must succeed and leave them alone.
///
/// The sibling upgrade test above only covers the CREATE path, against a
/// registry where the table is absent. That is the strictly easier case,
/// and the difference is not academic: adding a `NOT NULL` column with a
/// non-constant default to this table renders on SQLite as
///
/// ```sql
/// ALTER TABLE "rustango_org_hosts"
///   ADD COLUMN "updated_at" TEXT DEFAULT CURRENT_TIMESTAMP NOT NULL
/// ```
///
/// which SQLite rejects — "Cannot add a column with non-constant
/// default" — but *only* once the table has rows. On an empty table the
/// identical statement succeeds. A CREATE-path test therefore stays green
/// while every populated deployment fails to migrate, so any column added
/// to this model needs the assertion below and not that one.
#[tokio::test]
async fn a_populated_host_table_survives_a_re_migrate() {
    let _guard = cache_lock().lock().await;
    let dir = tempdir_unique("org-hosts-remigrate");
    std::fs::create_dir_all(dir.join("migrations")).expect("migrations dir");
    let db = dir.join("registry.db");
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect(&format!("sqlite:{}?mode=rwc", db.display()))
            .await
            .expect("sqlite"),
    );
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework tables");
    let mut org = Org {
        id: rustango::sql::Auto::Unset,
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some("acme.example.com".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&pool).await.expect("insert org");

    // Real operator data in the table before the upgrade runs.
    add_host(&pool, "acme", "kept.acme.com").await.expect("add");
    add_host(&pool, "acme", "also-kept.acme.com")
        .await
        .expect("add");

    rustango::tenancy::migrate_registry_pool(&pool, &dir.join("migrations"))
        .await
        .expect("migrate must succeed against a POPULATED rustango_org_hosts");

    let hosts = list_for_org(&pool, "acme").await.expect("list");
    let names: Vec<&str> = hosts.iter().map(|h| h.hostname.as_str()).collect();
    assert!(
        names.contains(&"kept.acme.com") && names.contains(&"also-kept.acme.com"),
        "existing host rows must survive the migrate, got {names:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The fingerprint contract, asserted directly rather than through the
/// resolver: every mutation this module can perform has to move it.
///
/// The toggle is the interesting one — it changes neither the row count
/// nor the max id, so a `(count, max_id)` fingerprint would miss it
/// entirely and a disable on one pod would never reach the others.
#[tokio::test]
async fn every_mutation_moves_the_fingerprint() {
    let _guard = cache_lock().lock().await;
    use rustango::tenancy::org_host::generation;
    let pool = registry_with_org().await;

    let empty = generation(&pool).await.expect("generation");
    assert_eq!(
        (empty.count, empty.enabled, empty.max_id),
        (0, 0, 0),
        "an empty table must fingerprint as all-zero, not error"
    );

    add_host(&pool, "acme", "fp.acme.com").await.expect("add");
    let after_add = generation(&pool).await.expect("generation");
    assert_ne!(after_add, empty, "an add must move the fingerprint");
    assert_eq!((after_add.count, after_add.enabled), (1, 1));

    set_host_enabled(&pool, "acme", "fp.acme.com", false)
        .await
        .expect("disable");
    let after_toggle = generation(&pool).await.expect("generation");
    assert_ne!(
        after_toggle, after_add,
        "a toggle must move the fingerprint — this is the mutation a \
         count-and-max-id fingerprint misses"
    );
    assert_eq!(
        (after_toggle.count, after_toggle.enabled),
        (1, 0),
        "the row is still there, just not enabled"
    );

    remove_host(&pool, "acme", "fp.acme.com")
        .await
        .expect("remove");
    let after_remove = generation(&pool).await.expect("generation");
    assert_ne!(
        after_remove, after_toggle,
        "a remove must move the fingerprint"
    );
    assert_eq!(after_remove.count, 0);
}

/// Two opposite toggles inside one poll interval must still register.
///
/// This is the case a plain enabled-*count* misses: disable one host and
/// enable another — an operator swapping which domain is live — and the
/// count goes −1 then +1 while `count` and `max_id` never move. The
/// fingerprint would sit still and neither change would reach the other
/// pods until the 30s TTL expired.
///
/// `enabled_id_sum` is what catches it, by identifying which rows are on
/// rather than how many. Dropping that term fails this test and leaves
/// every other one green.
#[tokio::test]
async fn compensating_toggles_still_move_the_fingerprint() {
    let _guard = cache_lock().lock().await;
    use rustango::tenancy::org_host::generation;
    let pool = registry_with_org().await;

    add_host(&pool, "acme", "aaa.acme.com").await.expect("add");
    add_host(&pool, "acme", "bbb.acme.com").await.expect("add");
    set_host_enabled(&pool, "acme", "bbb.acme.com", false)
        .await
        .expect("disable bbb");

    let before = generation(&pool).await.expect("generation");

    // The swap: aaa off, bbb on, inside one interval.
    set_host_enabled(&pool, "acme", "aaa.acme.com", false)
        .await
        .expect("disable aaa");
    set_host_enabled(&pool, "acme", "bbb.acme.com", true)
        .await
        .expect("enable bbb");

    let after = generation(&pool).await.expect("generation");
    assert_eq!(
        (after.count, after.enabled, after.max_id),
        (before.count, before.enabled, before.max_id),
        "precondition: count / enabled / max_id are all unchanged by the \
         swap — that is exactly why the extra term is needed"
    );
    assert_ne!(
        after, before,
        "a disable+enable pair must still move the fingerprint"
    );
}

/// A failing host-table lookup must back off, not re-ask on every request.
///
/// The miss path caches; the error path cannot, because an error is not
/// evidence that the host is unregistered. That left it uncached and
/// unthrottled — one failing `SELECT` per request, aimed at a registry
/// that is by definition already unhealthy. Measured against a renamed
/// table with the backoff removed: 25,987 failing queries for 25,958
/// requests.
///
/// Asserted without counting queries, which SQLite will not tell us:
/// restore the table *and* clear the resolution cache, then resolve
/// again. A resolver that queried would find the row and return the
/// tenant; one that is backed off cannot, so `None` here is positive
/// proof no query was issued.
#[tokio::test]
async fn a_failing_host_lookup_backs_off_instead_of_querying_per_request() {
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    add_host(&pool, "acme", "backoff.acme.com")
        .await
        .expect("add");

    let Pool::Sqlite(sq) = &pool else {
        unreachable!("sqlite-only test")
    };

    // Take the table away and resolve — fails open, and arms the backoff.
    sqlx::query("ALTER TABLE rustango_org_hosts RENAME TO hidden_hosts")
        .execute(sq)
        .await
        .expect("rename away");
    rustango::tenancy::invalidate_host_cache();
    let during = RegisteredHostResolver
        .resolve(&parts_for_host("backoff.acme.com"), &pool)
        .await
        .expect("a missing table must not error the request");
    assert!(
        during.is_none(),
        "fail-open: a missing table means no match"
    );

    // Put it back, and clear the cache so the cache cannot explain the
    // next result. The row is now present and resolvable.
    sqlx::query("ALTER TABLE hidden_hosts RENAME TO rustango_org_hosts")
        .execute(sq)
        .await
        .expect("rename back");
    rustango::tenancy::invalidate_host_cache();

    let still_backed_off = RegisteredHostResolver
        .resolve(&parts_for_host("backoff.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(
        still_backed_off.is_none(),
        "the resolver must still be backed off — returning the tenant here \
         would mean it queried, which is the per-request storm this guards"
    );

    // Clearing the backoff (as the interval elapsing would) restores it.
    rustango::testkit::reset_host_generation();
    rustango::tenancy::invalidate_host_cache();
    let recovered = RegisteredHostResolver
        .resolve(&parts_for_host("backoff.acme.com"), &pool)
        .await
        .expect("resolve");
    assert_eq!(
        recovered.map(|o| o.slug),
        Some("acme".to_owned()),
        "once the backoff lapses the resolver must query again and succeed"
    );
}

async fn table_exists(pool: &Pool, table: &str) -> bool {
    let Pool::Sqlite(sq) = pool else {
        unreachable!("sqlite-only test")
    };
    sqlx::query_scalar::<_, i64>("select count(*) from sqlite_master where type='table' and name=?")
        .bind(table)
        .fetch_one(sq)
        .await
        .unwrap_or(0)
        > 0
}

fn tempdir_unique(prefix: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

// ---------------------------------------------------------------- routing

/// Build request `Parts` carrying a `Host` header, the way the resolver
/// sees a real request.
fn parts_for_host(host: &str) -> http::request::Parts {
    let req = http::Request::builder()
        .uri("/")
        .header(http::header::HOST, host)
        .body(())
        .expect("request");
    req.into_parts().0
}

#[tokio::test]
async fn a_registered_host_routes_to_its_tenant() {
    let _guard = cache_lock().lock().await;
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();
    add_host(&pool, "acme", "routes.acme.com")
        .await
        .expect("add");

    let got = RegisteredHostResolver
        .resolve(&parts_for_host("routes.acme.com"), &pool)
        .await
        .expect("resolve");
    assert_eq!(got.map(|o| o.slug), Some("acme".to_owned()));
}

#[tokio::test]
async fn an_unregistered_host_resolves_to_nothing() {
    let _guard = cache_lock().lock().await;
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();
    let got = RegisteredHostResolver
        .resolve(&parts_for_host("nobody.example.com"), &pool)
        .await
        .expect("resolve");
    assert!(got.is_none());
}

/// A disabled host must stop routing without being unbound — that is the
/// difference between "parked" and "removed".
#[tokio::test]
async fn a_disabled_host_stops_routing() {
    let _guard = cache_lock().lock().await;
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();
    add_host(&pool, "acme", "parked.acme.com")
        .await
        .expect("add");
    set_host_enabled(&pool, "acme", "parked.acme.com", false)
        .await
        .expect("disable");

    let got = RegisteredHostResolver
        .resolve(&parts_for_host("parked.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(got.is_none(), "a disabled host must not route");
}

/// The cache must not outlive a mutation. Without the invalidation hook an
/// operator would add a host and then watch it 404 for the TTL, which is
/// exactly the kind of thing that gets diagnosed as "it doesn't work".
#[tokio::test]
async fn mutations_invalidate_the_resolution_cache() {
    let _guard = cache_lock().lock().await;
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();

    // Prime the NEGATIVE entry first — the case that would otherwise stick.
    let miss = RegisteredHostResolver
        .resolve(&parts_for_host("later.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(miss.is_none());

    add_host(&pool, "acme", "later.acme.com")
        .await
        .expect("add");
    let hit = RegisteredHostResolver
        .resolve(&parts_for_host("later.acme.com"), &pool)
        .await
        .expect("resolve");
    assert_eq!(
        hit.map(|o| o.slug),
        Some("acme".to_owned()),
        "adding a host must invalidate its cached miss"
    );

    // ...and removing it must invalidate the positive entry just as fast.
    remove_host(&pool, "acme", "later.acme.com")
        .await
        .expect("remove");
    let gone = RegisteredHostResolver
        .resolve(&parts_for_host("later.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(
        gone.is_none(),
        "removing a host must invalidate its cached hit"
    );
}

// ------------------------------------------------- cross-process staleness

/// Write straight to the table, the way a **different pod** would: no call
/// to this process's `invalidate_host_cache`, because that pod has its own.
async fn insert_host_behind_the_cache(pool: &Pool, org_slug: &str, hostname: &str) {
    let org = Org::objects()
        .where_(rustango::tenancy::Org::slug.eq(org_slug.to_owned()))
        .first(pool)
        .await
        .expect("query")
        .expect("org");
    let mut row = rustango::tenancy::OrgHost {
        id: rustango::sql::Auto::Unset,
        org_id: org.id.get().copied().unwrap_or_default(),
        hostname: hostname.to_owned(),
        enabled: true,
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_pool(pool).await.expect("insert");
}

/// The gap local invalidation cannot close: another pod binds a host, and
/// this one is still answering from a cached miss. The generation check has
/// to notice and drop the cache.
#[tokio::test]
async fn a_host_added_by_another_pod_is_picked_up() {
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();
    rustango::testkit::reset_host_generation();

    // This pod caches the miss.
    let miss = RegisteredHostResolver
        .resolve(&parts_for_host("otherpod.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(miss.is_none());

    // Another pod binds it — nothing invalidates *this* process.
    insert_host_behind_the_cache(&pool, "acme", "otherpod.acme.com").await;

    // Without the generation check this stays None until the 30s TTL.
    rustango::testkit::expire_host_generation();
    let hit = RegisteredHostResolver
        .resolve(&parts_for_host("otherpod.acme.com"), &pool)
        .await
        .expect("resolve");
    assert_eq!(
        hit.map(|o| o.slug),
        Some("acme".to_owned()),
        "a host bound by another pod must invalidate this pod's cached miss"
    );
}

/// A toggle changes neither the row count nor the max id, so it is the
/// mutation a naive fingerprint misses — hence the `enabled` term.
///
/// Swapping the fingerprint back to `(count, max_id)` fails this test and
/// leaves every other one green, which is the property that makes the
/// third term worth carrying.
#[tokio::test]
async fn a_deactivation_by_another_pod_is_picked_up() {
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();
    rustango::testkit::reset_host_generation();

    insert_host_behind_the_cache(&pool, "acme", "toggle.acme.com").await;
    rustango::testkit::expire_host_generation();
    let hit = RegisteredHostResolver
        .resolve(&parts_for_host("toggle.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(hit.is_some(), "precondition: it routes while enabled");

    // Another pod deactivates it — count and max(id) are unchanged.
    let mut row = rustango::tenancy::OrgHost::objects()
        .where_(rustango::tenancy::OrgHost::hostname.eq("toggle.acme.com".to_owned()))
        .first(&pool)
        .await
        .expect("query")
        .expect("row");
    row.enabled = false;
    row.save_pool(&pool).await.expect("save");

    rustango::testkit::expire_host_generation();
    let gone = RegisteredHostResolver
        .resolve(&parts_for_host("toggle.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(
        gone.is_none(),
        "a deactivation elsewhere must reach this pod — the fingerprint has \
         to include a timestamp, since count and max(id) do not move"
    );
}
