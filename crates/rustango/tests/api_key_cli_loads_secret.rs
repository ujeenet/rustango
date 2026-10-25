#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! `create-api-key` only loads the server's key; it never writes one.
//!
//! One test in its own binary: it moves the process cwd and needs the
//! API key secret unset.

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

#[tokio::test]
async fn create_api_key_never_writes_a_key_file() {
    if std::env::var_os("RUSTANGO_SESSION_SECRET").is_some() {
        return;
    }
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
    let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
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
    run(argv(&["create-api-key", "acme", "bob"]))
        .await
        .expect("the server's key file is used");
}
