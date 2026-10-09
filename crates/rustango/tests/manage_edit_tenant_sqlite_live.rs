#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! `edit-tenant` (#1344).
//!
//! Changing a tenant's routing was console-only, so moving one to a new
//! hostname or rotating its credential meant a browser pointed at
//! production.
//!
//! The risk in doing it from a second surface is not the UPDATE — it is
//! forgetting what has to follow it. `tenancy::org_edit::apply` drops the
//! cached `Org` after the write, because resolution serves from that
//! cache and a stale read reports success while the next request still
//! uses the old row.

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::{sqlx, FetcherPool as _, Pool};
use rustango::tenancy::{Org, TenantPools};

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

    async fn tenant(&self, slug: &str) {
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
            "--display-name",
            "Original",
            "--no-migrate",
        ])
        .await
        .unwrap_or_else(|e| panic!("create {slug}: {e}"));
    }

    async fn org(&self, slug: &str) -> Org {
        Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&self.registry)
            .await
            .expect("read org")
            .into_iter()
            .next()
            .expect("the org")
    }

    async fn has_org(&self, slug: &str) -> bool {
        !Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&self.registry)
            .await
            .expect("read org")
            .is_empty()
    }
}

#[tokio::test]
async fn editing_one_field_leaves_the_others_alone() {
    let b = boot().await;
    b.tenant("acme").await;

    let out = b
        .run(&["edit-tenant", "acme", "--host-pattern", "shop.example.com"])
        .await
        .expect("edit");
    assert!(
        out.contains("host_pattern"),
        "should name what changed: {out}"
    );

    let org = b.org("acme").await;
    assert_eq!(org.host_pattern.as_deref(), Some("shop.example.com"));
    assert_eq!(
        org.display_name, "Original",
        "an unmentioned field must not be wiped"
    );
}

/// The stored value has to match a `Host` header byte-for-byte, so it is
/// normalized on the way in rather than as typed.
#[tokio::test]
async fn a_host_pattern_is_normalized() {
    let b = boot().await;
    b.tenant("acme").await;

    b.run(&["edit-tenant", "acme", "--host-pattern", "SHOP.Example.COM"])
        .await
        .expect("edit");
    assert_eq!(
        b.org("acme").await.host_pattern.as_deref(),
        Some("shop.example.com")
    );
}

#[tokio::test]
async fn a_value_the_resolver_could_never_match_is_refused() {
    let b = boot().await;
    b.tenant("acme").await;

    // A port in the pattern: the header is matched with the port stripped.
    let err = b
        .run(&["edit-tenant", "acme", "--host-pattern", "shop.test:8443"])
        .await
        .expect_err("port in pattern");
    assert!(err.contains("port"), "{err}");

    // A path prefix the PathPrefixResolver cannot produce.
    let err = b
        .run(&["edit-tenant", "acme", "--path-prefix", "no-leading-slash"])
        .await
        .expect_err("bad prefix");
    assert!(err.contains('/'), "{err}");

    let err = b
        .run(&["edit-tenant", "acme", "--port", "70000"])
        .await
        .expect_err("out of range");
    assert!(err.contains("65535"), "{err}");

    // And none of it was written.
    let org = b.org("acme").await;
    assert!(org.host_pattern.is_none(), "nothing should have landed");
}

#[tokio::test]
async fn clear_empties_a_field_that_was_set() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&["edit-tenant", "acme", "--host-pattern", "shop.example.com"])
        .await
        .expect("set");

    b.run(&["edit-tenant", "acme", "--clear", "host-pattern"])
        .await
        .expect("clear");
    assert!(
        b.org("acme").await.host_pattern.is_none(),
        "should be NULL, not an empty string"
    );
}

#[tokio::test]
async fn activate_and_deactivate_flip_the_column() {
    let b = boot().await;
    b.tenant("acme").await;
    assert!(b.org("acme").await.active);

    b.run(&["edit-tenant", "acme", "--deactivate"])
        .await
        .expect("deactivate");
    assert!(!b.org("acme").await.active);

    b.run(&["edit-tenant", "acme", "--activate"])
        .await
        .expect("activate");
    assert!(b.org("acme").await.active);
}

/// Rotating the URL says when servers switch; changing a display name
/// must not, or every edit would throw away warm connections.
#[tokio::test]
async fn only_a_real_url_change_evicts_the_pool() {
    let b = boot().await;
    b.tenant("acme").await;

    let out = b
        .run(&["edit-tenant", "acme", "--display-name", "Acme Inc"])
        .await
        .expect("rename");
    assert!(!out.contains("servers switch"), "{out}");

    let fresh = b._tmp.path().join("acme2.db");
    let out = b
        .run(&[
            "edit-tenant",
            "acme",
            "--database-url",
            &format!("sqlite://{}?mode=rwc", fresh.display()),
        ])
        .await
        .expect("rotate");
    assert!(out.contains("servers switch"), "{out}");

    // Re-supplying the same URL is not a rotation.
    let same = b.org("acme").await.database_url.expect("a url");
    let out = b
        .run(&["edit-tenant", "acme", "--database-url", &same])
        .await
        .expect("no-op rotate");
    assert!(
        !out.contains("servers switch"),
        "unchanged is not a rotation: {out}"
    );
}

/// `--activate --deactivate` used to resolve last-wins and take the tenant
/// **offline** while printing `updated` and exiting 0 (#1355).
#[tokio::test]
async fn contradictory_activation_flags_are_refused() {
    let b = boot().await;
    b.tenant("acme").await;
    assert!(b.org("acme").await.active);

    for args in [
        vec!["edit-tenant", "acme", "--activate", "--deactivate"],
        vec!["edit-tenant", "acme", "--deactivate", "--activate"],
    ] {
        let err = b.run(&args).await.expect_err("contradiction");
        assert!(err.contains("contradict"), "{args:?}: {err}");
    }

    assert!(
        b.org("acme").await.active,
        "a refused edit must not have changed anything"
    );

    // Repeating one direction is not a contradiction.
    b.run(&["edit-tenant", "acme", "--deactivate", "--deactivate"])
        .await
        .expect("same direction twice");
    assert!(!b.org("acme").await.active);
}

/// `menu` off a terminal used to print 50 lines and exit 0, so a mistyped
/// command in CI looked like a successful step (#1357).
#[tokio::test]
async fn the_menu_refuses_to_run_without_a_terminal() {
    let b = boot().await;
    for verb in ["menu", "actions"] {
        let err = b.run(&[verb]).await.expect_err("no terminal under test");
        assert!(
            err.contains("terminal"),
            "`{verb}` should say why it cannot run: {err}"
        );
    }
}

#[tokio::test]
async fn an_edit_with_nothing_to_change_is_refused() {
    let b = boot().await;
    b.tenant("acme").await;

    let err = b.run(&["edit-tenant", "acme"]).await.expect_err("empty");
    assert!(err.contains("at least one field"), "{err}");
}

#[tokio::test]
async fn an_unknown_tenant_is_named() {
    let b = boot().await;
    let err = b
        .run(&["edit-tenant", "nosuchtenant", "--activate"])
        .await
        .expect_err("unknown");
    assert!(err.contains("nosuchtenant"), "{err}");
}

/// #1931: a host another tenant answers on, as its base or an extra
/// host, would route by row order, so it is refused.
#[tokio::test]
async fn a_host_another_tenant_uses_is_refused() {
    let b = boot().await;
    b.tenant("acme").await;
    b.tenant("beta").await;
    b.run(&["edit-tenant", "acme", "--host-pattern", "shop.example.com"])
        .await
        .expect("first claim");
    b.run(&["add-host", "acme", "extra.example.com"])
        .await
        .expect("extra host");

    for taken in ["SHOP.example.com", "extra.example.com"] {
        let err = b
            .run(&["edit-tenant", "beta", "--host-pattern", taken])
            .await
            .expect_err("claimed by acme");
        assert!(err.contains("another tenant"), "{taken}: {err}");
    }
    assert!(b.org("beta").await.host_pattern.is_none());

    // Re-saving a tenant's own host is not a clash.
    b.run(&["edit-tenant", "acme", "--host-pattern", "shop.example.com"])
        .await
        .expect("own host");
}

/// A path prefix or port another tenant routes on is refused on both
/// write paths, like a host.
#[tokio::test]
async fn a_prefix_or_port_another_tenant_uses_is_refused() {
    let b = boot().await;
    b.tenant("acme").await;
    b.tenant("beta").await;
    b.run(&[
        "edit-tenant",
        "acme",
        "--path-prefix",
        "/shop",
        "--port",
        "8443",
    ])
    .await
    .expect("first claim");

    for args in [["--path-prefix", "/shop"], ["--port", "8443"]] {
        let err = b
            .run(&["edit-tenant", "beta", args[0], args[1]])
            .await
            .expect_err("claimed by acme");
        assert!(err.contains("another tenant"), "{args:?}: {err}");
    }
    let db = b._tmp.path().join("gamma.db");
    let err = b
        .run(&[
            "create-tenant",
            "gamma",
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &format!("sqlite://{}?mode=rwc", db.display()),
            "--path-prefix",
            "/shop",
            "--no-migrate",
        ])
        .await
        .expect_err("prefix claimed by acme");
    assert!(err.contains("another tenant"), "{err}");

    // Re-saving a tenant's own prefix is not a clash.
    b.run(&["edit-tenant", "acme", "--path-prefix", "/shop"])
        .await
        .expect("own prefix");
}

/// A tenant may promote its own extra host to its base host, and a host
/// row stored before lowercasing still clashes.
#[tokio::test]
async fn own_extra_hosts_are_not_a_clash_and_case_is_ignored() {
    let b = boot().await;
    b.tenant("acme").await;
    b.tenant("beta").await;
    b.run(&["add-host", "acme", "extra.example.com"])
        .await
        .expect("extra host");
    b.run(&["edit-tenant", "acme", "--host-pattern", "extra.example.com"])
        .await
        .expect("acme's own extra host");

    let mut legacy = rustango::tenancy::OrgHost {
        id: rustango::sql::Auto::Unset,
        org_id: b.org("acme").await.id.get().copied().unwrap_or_default(),
        hostname: "LEGACY.example.com".to_owned(),
        enabled: true,
        created_at: rustango::sql::Auto::Unset,
    };
    legacy.insert_pool(&b.registry).await.expect("legacy row");
    let err = b
        .run(&[
            "edit-tenant",
            "beta",
            "--host-pattern",
            "legacy.example.com",
        ])
        .await
        .expect_err("claimed by acme in another case");
    assert!(err.contains("another tenant"), "{err}");
}

/// #2320: every path that writes a tenant URL refuses the registry's own
/// database, or tenant migrations run over the registry.
#[tokio::test]
async fn no_path_points_a_tenant_at_the_registry_database() {
    use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
    use rustango::tenancy::{BackendKind, StorageMode};
    let b = boot().await;
    b.tenant("acme").await;
    let registry = b.url.replace("?mode=rwc", "");

    let err = b
        .run(&["edit-tenant", "acme", "--database-url", &registry])
        .await
        .expect_err("edit to the registry");
    assert!(err.contains("registry's own database"), "{err}");
    assert_ne!(
        b.org("acme").await.database_url.as_deref(),
        Some(&*registry)
    );

    let err = create_tenant(
        b.pools.as_ref(),
        &b.url,
        b.migrations.path(),
        "globex",
        CreateTenantOpts {
            mode: StorageMode::Database,
            backend: BackendKind::Sqlite,
            database_url: Some(registry.clone()),
            no_migrate: true,
            ..CreateTenantOpts::default()
        },
    )
    .await
    .expect_err("create on the registry");
    assert!(err.to_string().contains("registry's own database"), "{err}");
    assert!(
        !b.has_org("globex").await,
        "no Org row for a refused create"
    );

    let err = b
        .run(&[
            "create-tenant",
            "initech",
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &registry,
            "--no-migrate",
        ])
        .await
        .expect_err("CLI create on the registry");
    assert!(err.contains("registry's own database"), "{err}");
    assert!(
        !b.has_org("initech").await,
        "no Org row for a refused create"
    );
}
