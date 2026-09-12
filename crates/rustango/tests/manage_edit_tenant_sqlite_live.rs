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

/// Rotating the URL says the pool was evicted; changing a display name
/// must not, or every edit would throw away warm connections.
#[tokio::test]
async fn only_a_real_url_change_evicts_the_pool() {
    let b = boot().await;
    b.tenant("acme").await;

    let out = b
        .run(&["edit-tenant", "acme", "--display-name", "Acme Inc"])
        .await
        .expect("rename");
    assert!(!out.contains("evicted"), "{out}");

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
    assert!(out.contains("evicted"), "{out}");

    // Re-supplying the same URL is not a rotation.
    let same = b.org("acme").await.database_url.expect("a url");
    let out = b
        .run(&["edit-tenant", "acme", "--database-url", &same])
        .await
        .expect("no-op rotate");
    assert!(
        !out.contains("evicted"),
        "unchanged is not a rotation: {out}"
    );
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
