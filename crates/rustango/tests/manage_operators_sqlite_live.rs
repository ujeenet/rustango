#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! `list-operators` and `set-operator-active` (#1344).
//!
//! Disabling a departed operator was a console-only action. The rules that
//! make it safe — no self-deactivation, never the last active one — lived in
//! the console handler, so a CLI verb would have restated them and the copy
//! that drifted would be the one that locked everybody out.
//!
//! These assert the CLI enforces the same rules, because it calls the same
//! `tenancy::operators` engine the console now calls.

use std::sync::Arc;

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{operators as ops, TenantPools};

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

    async fn operator(&self, username: &str) {
        self.run(&[
            "create-operator",
            username,
            "--password",
            "correct-horse-42",
        ])
        .await
        .unwrap_or_else(|e| panic!("create {username}: {e}"));
    }
}

#[tokio::test]
async fn list_shows_who_exists_and_who_is_active() {
    let b = boot().await;
    b.operator("ada").await;
    b.operator("grace").await;

    let out = b.run(&["list-operators"]).await.expect("list");
    assert!(out.contains("ada") && out.contains("grace"), "{out}");
    assert!(out.contains("2 operator(s), 2 active"), "{out}");
}

#[tokio::test]
async fn an_empty_registry_says_so_rather_than_printing_a_bare_header() {
    let b = boot().await;
    let out = b.run(&["list-operators"]).await.expect("list");
    assert!(out.contains("no operators"), "{out}");
    assert!(
        out.contains("create-operator"),
        "should say what to do: {out}"
    );
}

#[tokio::test]
async fn deactivating_and_reactivating_round_trip() {
    let b = boot().await;
    b.operator("ada").await;
    b.operator("grace").await;

    let out = b
        .run(&["set-operator-active", "grace", "--off"])
        .await
        .expect("deactivate");
    assert!(out.contains("deactivated"), "{out}");

    let listed = b.run(&["list-operators"]).await.expect("list");
    assert!(listed.contains("2 operator(s), 1 active"), "{listed}");

    b.run(&["set-operator-active", "grace", "--on"])
        .await
        .expect("reactivate");
    let listed = b.run(&["list-operators"]).await.expect("list");
    assert!(listed.contains("2 operator(s), 2 active"), "{listed}");
}

/// The rule that matters: the CLI must not be a way around the lockout
/// guard the console enforces.
#[tokio::test]
async fn the_last_active_operator_cannot_be_deactivated() {
    let b = boot().await;
    b.operator("ada").await;

    let err = b
        .run(&["set-operator-active", "ada", "--off"])
        .await
        .expect_err("lockout");
    assert!(err.contains("last active operator"), "{err}");

    // And nothing was written.
    let still = ops::by_username(&b.registry, "ada").await.expect("read");
    assert!(still.active, "the operator must still be active");
}

/// …including when the others are already off, which is the state a
/// one-at-a-time deactivation walks into.
#[tokio::test]
async fn deactivating_down_to_the_last_one_stops_at_one() {
    let b = boot().await;
    b.operator("ada").await;
    b.operator("grace").await;

    b.run(&["set-operator-active", "grace", "--off"])
        .await
        .expect("first");
    let err = b
        .run(&["set-operator-active", "ada", "--off"])
        .await
        .expect_err("second should be refused");
    assert!(err.contains("last active operator"), "{err}");
}

/// A no-op is reported, not failed — a re-run script should not go red.
#[tokio::test]
async fn setting_the_state_it_already_has_is_not_an_error() {
    let b = boot().await;
    b.operator("ada").await;

    let out = b
        .run(&["set-operator-active", "ada", "--on"])
        .await
        .expect("already active");
    assert!(out.contains("already active"), "{out}");
}

/// Passing both directions used to resolve last-wins, which is the same
/// guess the required-direction rule exists to prevent (#1355).
#[tokio::test]
async fn contradictory_directions_are_refused() {
    let b = boot().await;
    b.operator("ada").await;
    b.operator("grace").await;

    for args in [
        vec!["set-operator-active", "grace", "--on", "--off"],
        vec!["set-operator-active", "grace", "--off", "--on"],
    ] {
        let err = b.run(&args).await.expect_err("contradiction");
        assert!(err.contains("contradict"), "{args:?}: {err}");
    }
    assert!(
        ops::by_username(&b.registry, "grace")
            .await
            .expect("read")
            .active,
        "a refused command must not have changed anything"
    );
}

#[tokio::test]
async fn a_direction_is_required_and_an_unknown_operator_is_named() {
    let b = boot().await;
    b.operator("ada").await;

    let err = b
        .run(&["set-operator-active", "ada"])
        .await
        .expect_err("no direction");
    assert!(err.contains("--on") && err.contains("--off"), "{err}");

    let err = b
        .run(&["set-operator-active", "nobody", "--off"])
        .await
        .expect_err("unknown");
    assert!(err.contains("nobody"), "{err}");
}

/// The CLI and the console call one engine, so a change through either is
/// visible to the other.
#[tokio::test]
async fn the_verb_and_the_engine_agree() {
    let b = boot().await;
    b.operator("ada").await;
    b.operator("grace").await;

    b.run(&["set-operator-active", "grace", "--off"])
        .await
        .expect("deactivate");
    let via_engine = ops::by_username(&b.registry, "grace").await.expect("read");
    assert!(
        !via_engine.active,
        "the console would still show them active"
    );

    let mut target = via_engine;
    ops::set_active(&b.registry, &mut target, true, None)
        .await
        .expect("engine reactivate");
    let listed = b.run(&["list-operators"]).await.expect("list");
    assert!(listed.contains("2 operator(s), 2 active"), "{listed}");
}
