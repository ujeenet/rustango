//! Outside prod, an unset `RUSTANGO_SECRET_KEY` falls back to a key saved
//! in `var/` at the workspace root, so a fresh dev project can seal; prod
//! still needs the var. No `Cli` or server boot: a worker or a test seals too.

#![cfg(feature = "tenancy")]

use rustango::tenancy::SecretsResolver as _;

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn only_the_dev_tier_seals_without_the_env_var() {
    let _g = ENV.lock().await;
    std::env::remove_var("RUSTANGO_SECRET_KEY");
    std::env::set_var("RUSTANGO_ENV", "prod");
    assert!(
        rustango::tenancy::seal("postgres://u:p@h/db").is_err(),
        "prod sealed with a key nobody set"
    );
    std::env::set_var("RUSTANGO_ENV", "dev");
    let sealed = rustango::tenancy::seal("postgres://u:p@h/db").expect("dev seals");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("Cargo.lock").is_file())
        .expect("workspace root");
    let key = std::fs::read(root.join("var/.rustango_secret.key")).expect("key at the root");
    assert!(key.len() >= 32);
    let open = rustango::tenancy::ChainSecretsResolver::standard()
        .resolve(&sealed)
        .await
        .expect("opens");
    assert_eq!(open, "postgres://u:p@h/db");
    std::env::remove_var("RUSTANGO_ENV");
}
