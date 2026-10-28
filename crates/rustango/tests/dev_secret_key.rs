//! Outside prod, an unset `RUSTANGO_SECRET_KEY` falls back to a key saved
//! in `./var/`, so a fresh dev project can seal; prod still needs the var.

#![cfg(all(feature = "tenancy", feature = "manage"))]

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn boot(tier: &str) {
    std::env::set_var("RUSTANGO_ENV", tier);
    rustango::manage::Cli::new()
        .run_with_args(vec!["help".to_owned()])
        .await
        .expect("help");
}

#[tokio::test]
async fn only_the_dev_tier_seals_without_the_env_var() {
    let _g = ENV.lock().await;
    std::env::remove_var("RUSTANGO_SECRET_KEY");
    boot("prod").await;
    assert!(
        rustango::tenancy::seal("postgres://u:p@h/db").is_err(),
        "prod sealed with a key nobody set"
    );
    boot("dev").await;
    let sealed = rustango::tenancy::seal("postgres://u:p@h/db").expect("dev seals");
    let key = std::fs::read("./var/.rustango_secret.key").expect("key saved in ./var/");
    assert!(key.len() >= 32);
    use rustango::tenancy::SecretsResolver as _;
    let open = rustango::tenancy::ChainSecretsResolver::standard()
        .resolve(&sealed)
        .await
        .expect("opens");
    assert_eq!(open, "postgres://u:p@h/db");
    std::env::remove_var("RUSTANGO_ENV");
}
