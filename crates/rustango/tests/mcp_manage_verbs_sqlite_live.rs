//! Live integration test for the MCP **user-owned-keys** + **skill↔permission**
//! `manage` verbs on SQLite (epic #1013 follow-up).
//!
//! Drives the full CLI dispatch — `migrate-registry` → `create-tenant`
//! (database mode) → `create-user` → the new verbs:
//!   `create-user-key`, `list-user-keys`, `revoke-user-key`,
//!   `map-skill-permission`, `unmap-skill-permission`.
//!
//! Mirrors `tenancy_manage_sqlite_live.rs`'s setup so the verbs are
//! exercised through exactly the code path `cargo run -- <verb>` uses.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "mcp"))]

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

/// Build a sqlite tenant-pools handle backed by a tempfile DB (a real
/// on-disk file so successive acquisitions see the same data).
async fn sqlite_pools_in_tempfile() -> (TenantPools<sqlx::Sqlite>, String, tempfile::TempDir) {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let db_path = tmpdir.path().join("registry.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = sqlx::SqlitePool::connect(&url)
        .await
        .expect("sqlite connect");
    let pools = TenantPools::<sqlx::Sqlite>::new(pool);
    (pools, url, tmpdir)
}

/// Run one manage verb, returning captured stdout. Panics on error.
async fn run(
    pools: &TenantPools<sqlx::Sqlite>,
    url: &str,
    dir: &std::path::Path,
    argv: &[&str],
) -> String {
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools,
        url,
        dir,
        argv.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        &mut buf,
    )
    .await
    .unwrap_or_else(|e| panic!("verb {argv:?} failed: {e}"));
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run one manage verb expecting an error; returns the error string.
async fn run_err(
    pools: &TenantPools<sqlx::Sqlite>,
    url: &str,
    dir: &std::path::Path,
    argv: &[&str],
) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let err = rustango::tenancy::manage::run_with_writer(
        pools,
        url,
        dir,
        argv.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        &mut buf,
    )
    .await
    .expect_err("verb should fail");
    err.to_string()
}

#[tokio::test]
async fn user_key_and_skill_permission_verbs_round_trip_on_sqlite() {
    let (pools, url, tmpdir) = sqlite_pools_in_tempfile().await;
    let migrations_dir = tempfile::tempdir().expect("migrations tempdir");
    let dir = migrations_dir.path();

    // Registry schema.
    run(&pools, &url, dir, &["migrate-registry"]).await;

    // A database-mode tenant (schema-mode is PG-only). Its DB is migrated
    // with the framework tables (rustango_users, rustango_agents, …).
    let tenant_db = tmpdir.path().join("acme.db");
    let tenant_url = format!("sqlite://{}?mode=rwc", tenant_db.display());
    run(
        &pools,
        &url,
        dir,
        &[
            "create-tenant",
            "acme",
            "--mode",
            "database",
            "--database-url",
            &tenant_url,
        ],
    )
    .await;

    // A tenant user to own the key.
    run(
        &pools,
        &url,
        dir,
        &["create-user", "acme", "alice", "--password", "hunter2"],
    )
    .await;

    // ---- create-user-key: prints a one-time token + key id ----
    let out = run(
        &pools,
        &url,
        dir,
        &["create-user-key", "acme", "alice", "--label", "mylabel"],
    )
    .await;
    assert!(out.contains("token:"), "expected a token line, got:\n{out}");
    assert!(
        out.contains("won't be shown again"),
        "expected the one-time warning, got:\n{out}"
    );
    assert!(out.contains("mylabel"), "expected the label, got:\n{out}");
    // Extract the key id from `created key #<id> for user ...`.
    let key_id: i64 = out
        .split_once('#')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse key id from:\n{out}"));

    // ---- list-user-keys: shows the key + label ----
    let listed = run(&pools, &url, dir, &["list-user-keys", "acme", "alice"]).await;
    assert!(
        listed.contains(&format!("#{key_id}")),
        "expected key #{key_id} in list, got:\n{listed}"
    );
    assert!(
        listed.contains("mylabel"),
        "expected label in list, got:\n{listed}"
    );

    // ---- revoke-user-key: removes it ----
    let revoked = run(
        &pools,
        &url,
        dir,
        &["revoke-user-key", "acme", "alice", &key_id.to_string()],
    )
    .await;
    assert!(
        revoked.contains(&format!("revoked key #{key_id}")),
        "got:\n{revoked}"
    );
    let empty = run(&pools, &url, dir, &["list-user-keys", "acme", "alice"]).await;
    assert!(
        empty.contains("no keys for user `alice`"),
        "expected empty listing, got:\n{empty}"
    );

    // ---- unknown user is a friendly validation error ----
    let err = run_err(&pools, &url, dir, &["create-user-key", "acme", "nobody"]).await;
    assert!(
        err.contains("unknown user `nobody`"),
        "expected unknown-user error, got: {err}"
    );

    // ---- map-skill-permission is idempotent ----
    run(
        &pools,
        &url,
        dir,
        &["create-skill", "acme", "greeter", "--tools", "echo"],
    )
    .await;
    let mapped = run(
        &pools,
        &url,
        dir,
        &["map-skill-permission", "acme", "greeter", "greeter.use"],
    )
    .await;
    assert!(mapped.contains("mapped skill `greeter`"), "got:\n{mapped}");
    // Second call is a no-op (must not error).
    run(
        &pools,
        &url,
        dir,
        &["map-skill-permission", "acme", "greeter", "greeter.use"],
    )
    .await;

    // ---- unmap-skill-permission removes it ----
    let unmapped = run(
        &pools,
        &url,
        dir,
        &["unmap-skill-permission", "acme", "greeter", "greeter.use"],
    )
    .await;
    assert!(
        unmapped.contains("unmapped skill `greeter`"),
        "got:\n{unmapped}"
    );
}
