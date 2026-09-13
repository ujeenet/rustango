#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Hostname verbs, and that they write what the console writes (#1344).
//!
//! Binding an extra hostname was console-only, so it could not run in a
//! deploy hook, in CI, or on a box where nobody can open a browser. These
//! verbs call `tenancy::org_host` — the same engine the console posts to —
//! and this asserts the two really do land on one table rather than two
//! code paths that merely look alike.

use std::sync::Arc;

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{org_host, TenantPools};

struct Booted {
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    registry: Pool,
    url: String,
    _tmp: tempfile::TempDir,
    migrations: tempfile::TempDir,
}

async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool));
    let migrations = tempfile::tempdir().expect("migrations dir");

    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &url,
        migrations.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    let registry = pools.registry_pool();
    Booted {
        pools,
        registry,
        url,
        _tmp: tmp,
        migrations,
    }
}

impl Booted {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut buf: Vec<u8> = Vec::new();
        let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        rustango::tenancy::manage::run_with_writer(
            self.pools.as_ref(),
            &self.url,
            self.migrations.path(),
            argv,
            &mut buf,
        )
        .await
        .map(|()| String::from_utf8_lossy(&buf).into_owned())
        .map_err(|e| e.to_string())
    }

    /// A tenant with no storage to provision — the host table is in the
    /// registry, so routing can be set up without a tenant database.
    async fn tenant(&self, slug: &str) -> String {
        let db = self._tmp.path().join(format!("{slug}.db"));
        self.run(&[
            "create-tenant",
            slug,
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &format!("sqlite://{}?mode=rwc", db.display()),
            "--no-migrate",
        ])
        .await
        .expect("create the tenant");
        slug.to_owned()
    }
}

#[tokio::test]
async fn add_list_and_remove_round_trip() {
    let b = boot().await;
    let slug = b.tenant("acme").await;

    let out = b
        .run(&["add-host", &slug, "shop.example.com"])
        .await
        .expect("add");
    assert!(out.contains("shop.example.com"), "{out}");

    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(listed.contains("shop.example.com"), "{listed}");

    b.run(&["remove-host", &slug, "shop.example.com"])
        .await
        .expect("remove");
    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(
        !listed.contains("shop.example.com"),
        "should be gone: {listed}"
    );
}

/// The point of the slice: the verb and the console write the same rows.
#[tokio::test]
async fn the_verb_and_the_engine_see_one_table() {
    let b = boot().await;
    let slug = b.tenant("acme").await;

    // Written through the CLI…
    b.run(&["add-host", &slug, "a.example.com"])
        .await
        .expect("add");
    // …read through the engine the console calls.
    let via_engine = org_host::list_for_org(&b.registry, &slug)
        .await
        .expect("list_for_org");
    assert!(
        via_engine.iter().any(|h| h.hostname == "a.example.com"),
        "the console would not see the CLI's host: {via_engine:?}"
    );

    // And the other direction.
    org_host::add_host(&b.registry, &slug, "b.example.com")
        .await
        .expect("engine add");
    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(
        listed.contains("b.example.com"),
        "the CLI would not see the console's host: {listed}"
    );
}

/// Hostnames are normalized before storage, so the stored value matches the
/// `Host` header byte-for-byte. The verb must echo what was stored.
#[tokio::test]
async fn a_hostname_is_normalized_not_stored_as_typed() {
    let b = boot().await;
    let slug = b.tenant("acme").await;

    let out = b
        .run(&["add-host", &slug, "  SHOP.Example.COM  "])
        .await
        .expect("add");
    assert!(
        out.contains("shop.example.com"),
        "should echo the stored form: {out}"
    );
    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(listed.contains("shop.example.com"), "{listed}");
}

/// One hostname routes to one tenant, so the second claim must lose.
#[tokio::test]
async fn a_host_claimed_by_another_tenant_is_refused() {
    let b = boot().await;
    let one = b.tenant("acme").await;
    let two = b.tenant("globex").await;

    b.run(&["add-host", &one, "shared.example.com"])
        .await
        .expect("first claim");
    let err = b
        .run(&["add-host", &two, "shared.example.com"])
        .await
        .expect_err("second claim");
    assert!(err.contains("already registered"), "{err}");
}

#[tokio::test]
async fn parking_a_host_keeps_the_row() {
    let b = boot().await;
    let slug = b.tenant("acme").await;
    b.run(&["add-host", &slug, "shop.example.com"])
        .await
        .expect("add");

    let out = b
        .run(&["set-host-enabled", &slug, "shop.example.com", "--off"])
        .await
        .expect("park");
    assert!(out.contains("parked"), "{out}");

    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(
        listed.contains("shop.example.com") && listed.contains("false"),
        "the row should survive, disabled: {listed}"
    );

    b.run(&["set-host-enabled", &slug, "shop.example.com", "--on"])
        .await
        .expect("serve");
    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(listed.contains("true"), "{listed}");
}

/// Neither direction given is refused rather than guessed: picking one would
/// park a live host or serve a parked one.
#[tokio::test]
async fn set_host_enabled_refuses_to_guess_a_direction() {
    let b = boot().await;
    let slug = b.tenant("acme").await;
    b.run(&["add-host", &slug, "shop.example.com"])
        .await
        .expect("add");

    let err = b
        .run(&["set-host-enabled", &slug, "shop.example.com"])
        .await
        .expect_err("no direction");
    assert!(err.contains("--on") && err.contains("--off"), "{err}");
}

/// The base host lives on `Org.host_pattern`, not in this table — removing
/// it here would silently do nothing, so it is refused with the reason.
#[tokio::test]
async fn the_base_host_cannot_be_removed_here() {
    let b = boot().await;
    let slug = "acme";
    let db = b._tmp.path().join("acme.db");
    b.run(&[
        "create-tenant",
        slug,
        "--mode",
        "database",
        "--backend",
        "sqlite",
        "--database-url",
        &format!("sqlite://{}?mode=rwc", db.display()),
        "--host-pattern",
        "acme.example.com",
        "--no-migrate",
    ])
    .await
    .expect("create with a base host");

    let listed = b.run(&["list-hosts", slug]).await.expect("list");
    assert!(
        listed.contains("acme.example.com") && listed.contains("base"),
        "the base host should be listed and labelled: {listed}"
    );

    let err = b
        .run(&["remove-host", slug, "acme.example.com"])
        .await
        .expect_err("base host");
    assert!(err.contains("base host"), "{err}");
}

/// The error has to name the **tenant**, not the host. One `NotFound` for
/// both sent a mistyped slug looking at the host table (#1356), and this
/// test used to assert only that *something* was said, which is how it got
/// through.
#[tokio::test]
async fn an_unknown_tenant_is_named() {
    let b = boot().await;
    let slug = b.tenant("acme").await;
    for args in [
        vec!["list-hosts", "nosuchtenant"],
        vec!["add-host", "nosuchtenant", "x.test"],
        vec!["remove-host", "nosuchtenant", "x.test"],
        vec!["set-host-enabled", "nosuchtenant", "x.test", "--on"],
    ] {
        let err = b.run(&args).await.expect_err("unknown tenant");
        assert!(
            err.contains("nosuchtenant") && err.contains("tenant"),
            "{args:?} should name the missing tenant, got: {err}"
        );
    }

    // …and a missing *host* on a tenant that exists still says so.
    let err = b
        .run(&["remove-host", &slug, "nosuch.test"])
        .await
        .expect_err("unknown host");
    assert!(err.contains("host"), "{err}");
}

/// `--on --off` used to resolve last-wins and park a live host while
/// reporting success (#1355).
#[tokio::test]
async fn contradictory_directions_are_refused() {
    let b = boot().await;
    let slug = b.tenant("acme").await;
    b.run(&["add-host", &slug, "shop.example.com"])
        .await
        .expect("add");

    for args in [
        vec![
            "set-host-enabled",
            &slug,
            "shop.example.com",
            "--on",
            "--off",
        ],
        vec![
            "set-host-enabled",
            &slug,
            "shop.example.com",
            "--off",
            "--on",
        ],
    ] {
        let err = b.run(&args).await.expect_err("contradiction");
        assert!(err.contains("contradict"), "{args:?}: {err}");
    }

    // Repeating the *same* direction is harmless.
    b.run(&[
        "set-host-enabled",
        &slug,
        "shop.example.com",
        "--off",
        "--off",
    ])
    .await
    .expect("same direction twice is fine");

    let listed = b.run(&["list-hosts", &slug]).await.expect("list");
    assert!(listed.contains("false"), "still parked: {listed}");
}

/// `--enabled TRUE` used to mean *off*: anything outside a lowercase
/// allow-list fell through to `false` (#1355).
#[tokio::test]
async fn enabled_parses_a_closed_set_case_insensitively() {
    let b = boot().await;
    let slug = b.tenant("acme").await;
    b.run(&["add-host", &slug, "shop.example.com"])
        .await
        .expect("add");

    for on in ["TRUE", "Yes", "ON", "1"] {
        b.run(&[
            "set-host-enabled",
            &slug,
            "shop.example.com",
            "--enabled",
            on,
        ])
        .await
        .unwrap_or_else(|e| panic!("--enabled {on}: {e}"));
        let listed = b.run(&["list-hosts", &slug]).await.expect("list");
        assert!(
            listed.contains("true"),
            "--enabled {on} should serve: {listed}"
        );
    }
    for off in ["FALSE", "No", "off", "0"] {
        b.run(&[
            "set-host-enabled",
            &slug,
            "shop.example.com",
            "--enabled",
            off,
        ])
        .await
        .unwrap_or_else(|e| panic!("--enabled {off}: {e}"));
        let listed = b.run(&["list-hosts", &slug]).await.expect("list");
        assert!(
            listed.contains("false"),
            "--enabled {off} should park: {listed}"
        );
    }

    let err = b
        .run(&[
            "set-host-enabled",
            &slug,
            "shop.example.com",
            "--enabled",
            "maybe",
        ])
        .await
        .expect_err("not a yes/no value");
    assert!(err.contains("maybe"), "{err}");
}
