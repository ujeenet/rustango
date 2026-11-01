#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! `create-api-key` only loads the server's key; it never writes one.
//!
//! Its own binary: it moves the process cwd and clears the key env vars
//! for this process only. The ignored test is its server-side child.

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

#[tokio::test]
async fn create_api_key_never_writes_a_key_file() {
    // A set secret or prod tier would skip the dev key file this test is about.
    std::env::remove_var("RUSTANGO_SESSION_SECRET");
    std::env::remove_var("RUSTANGO_ENV");
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_current_dir(tmp.path()).expect("cwd");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let raw = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = TenantPools::<sqlx::Sqlite>::new(raw);
    let migrations = tempfile::tempdir().expect("migrations dir");
    let run = |args: Vec<String>| {
        let pools = &pools;
        let url = &url;
        let dir = migrations.path().to_owned();
        async move {
            let mut buf: Vec<u8> = Vec::new();
            rustango::tenancy::manage::run_with_writer(pools, url, &dir, args, &mut buf)
                .await
                .map(|()| String::from_utf8_lossy(&buf).into_owned())
                .map_err(|e| e.to_string())
        }
    };
    let argv = |a: &[&str]| rustango::testkit::routed_args(a);
    run(argv(&["migrate-registry"])).await.expect("registry");
    let db = tmp.path().join("acme.db");
    run(argv(&[
        "create-tenant",
        "acme",
        "--mode",
        "database",
        "--backend",
        "sqlite",
        "--database-url",
        &format!("sqlite://{}?mode=rwc", db.display()),
    ]))
    .await
    .expect("tenant");
    run(argv(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ]))
    .await
    .expect("user");

    let key_file = tmp.path().join("var/.rustango_tenant_session.key");
    let _ = std::fs::remove_file(&key_file);
    let err = run(argv(&["create-api-key", "acme", "bob"]))
        .await
        .expect_err("a key no server holds");
    assert!(err.contains("RUSTANGO_SESSION_SECRET"), "{err}");
    assert!(!key_file.exists(), "the CLI wrote a session key file");

    std::fs::create_dir_all(key_file.parent().unwrap()).unwrap();
    std::fs::write(&key_file, [9u8; 32]).unwrap();
    let out = run(argv(&["create-api-key", "acme", "bob"]))
        .await
        .expect("the server's key file is used");
    let token = out
        .lines()
        .map(str::trim)
        .find(|l| l.len() == 41 && l.as_bytes()[8] == b'.')
        .unwrap_or_else(|| panic!("no token in {out}"));

    // A fresh process loads the key the way a server does, so a CLI and a
    // server that disagree on the key fail here.
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([SERVER_CHILD, "--exact", "--ignored", "--nocapture"])
        .current_dir(tmp.path())
        .env_remove("RUSTANGO_SESSION_SECRET")
        .env_remove("RUSTANGO_ENV")
        .env("API_KEY_CHILD_TOKEN", token)
        .env("API_KEY_CHILD_DB", &db)
        .output()
        .expect("spawn server process");
    let log = String::from_utf8_lossy(&child.stdout);
    assert!(
        child.status.success() && log.contains("1 passed"),
        "the server refused a CLI-minted key:\n{log}\n{}",
        String::from_utf8_lossy(&child.stderr)
    );
}

const SERVER_CHILD: &str = "cli_minted_key_verifies_in_a_server_process";

/// Run only by the test above, in its own process.
#[tokio::test]
#[ignore = "spawned by create_api_key_never_writes_a_key_file"]
async fn cli_minted_key_verifies_in_a_server_process() {
    use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend};
    let token = std::env::var("API_KEY_CHILD_TOKEN").expect("run by the parent test");
    let db = std::env::var("API_KEY_CHILD_DB").expect("run by the parent test");
    let pool = rustango::sql::Pool::connect(&format!("sqlite://{db}"))
        .await
        .expect("tenant db");
    let addr: std::net::SocketAddr = "10.64.0.1:1".parse().unwrap();
    let (parts, ()) = axum::http::Request::builder()
        .header("authorization", format!("Bearer {token}"))
        .extension(rustango::tenancy::TenantSlug::new(1, "acme"))
        .extension(axum::extract::ConnectInfo(addr))
        .body(())
        .unwrap()
        .into_parts();
    let user = ApiKeyBackend.authenticate(&parts, &pool).await;
    assert!(
        matches!(&user, Ok(Some(u)) if u.username == "bob"),
        "{:?}",
        user.map(|u| u.map(|u| u.username))
    );
}
