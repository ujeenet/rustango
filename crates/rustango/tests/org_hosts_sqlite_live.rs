//! Extra tenant hostnames (`rustango_org_hosts`) against a real registry.
//!
//! The rules worth pinning are the ones a unit test cannot reach: a host
//! may be bound and unbound freely, but the tenant's **base** host — the
//! `Org.host_pattern` written by `create-tenant` — must survive every
//! removal path, because a tenant with no host is unreachable and there is
//! no UI to put it back.
//! NOTE on isolation: the resolver's cache is process-global and keyed by
//! hostname, so each test below uses a hostname of its own. Production has
//! one registry per process, which is why the cache needs no registry key.
#![cfg(all(feature = "tenancy", feature = "sqlite"))]

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
async fn registry_with_org() -> Pool {
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
    use rustango::sql::FetcherPool as _;
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
    use rustango::sql::FetcherPool as _;
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
    use rustango::sql::FetcherPool as _;
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
        updated_at: rustango::sql::Auto::Unset,
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
    rustango::tenancy::reset_generation_for_test();

    // This pod caches the miss.
    let miss = RegisteredHostResolver
        .resolve(&parts_for_host("otherpod.acme.com"), &pool)
        .await
        .expect("resolve");
    assert!(miss.is_none());

    // Another pod binds it — nothing invalidates *this* process.
    insert_host_behind_the_cache(&pool, "acme", "otherpod.acme.com").await;

    // Without the generation check this stays None until the 30s TTL.
    rustango::tenancy::expire_generation_for_test();
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
/// mutation a naive fingerprint misses — hence `max(updated_at)`.
#[tokio::test]
async fn a_deactivation_by_another_pod_is_picked_up() {
    use rustango::sql::FetcherPool as _;
    use rustango::tenancy::{OrgResolver, RegisteredHostResolver};
    let _guard = cache_lock().lock().await;
    let pool = registry_with_org().await;
    rustango::tenancy::invalidate_host_cache();
    rustango::tenancy::reset_generation_for_test();

    insert_host_behind_the_cache(&pool, "acme", "toggle.acme.com").await;
    rustango::tenancy::expire_generation_for_test();
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

    rustango::tenancy::expire_generation_for_test();
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
