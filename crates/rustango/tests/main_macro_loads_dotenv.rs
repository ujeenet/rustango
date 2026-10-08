//! `#[rustango::main]` reads `RUST_LOG` from `./.env` for its filter,
//! and nothing else from that file (#2204).
//!
//! Its own binary with one test: the global subscriber, env and cwd are
//! process-wide.

#![cfg(feature = "runtime")]

use rustango::__private_runtime::main_env_filter;
use tracing::Level;
use tracing_subscriber::EnvFilter;

#[rustango::main]
async fn run_app() -> (bool, bool) {
    (
        tracing::enabled!(Level::INFO),
        tracing::enabled!(Level::ERROR),
    )
}

fn filter_str(f: &str) -> String {
    EnvFilter::new(f).to_string()
}

#[test]
fn rust_log_from_dotenv_reaches_the_installed_filter() {
    std::env::remove_var("RUST_LOG");
    std::env::remove_var("RUSTANGO_ENV");
    std::env::remove_var("DATABASE_URL");
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(".env"),
        "RUSTANGO_ENV=prod\nRUST_LOG=error\nDATABASE_URL=x\n",
    )
    .expect("write .env");
    std::env::set_current_dir(tmp.path()).expect("chdir");

    let (info, error) = run_app();

    // The default filter is `info,sqlx=warn`; `info` off means `.env` won.
    assert!(!info, "RUST_LOG=error from .env must disable info events");
    assert!(error, "error events must stay enabled");
    // Only the filter is read: no other key reaches the environment.
    assert!(std::env::var_os("RUSTANGO_ENV").is_none());
    assert!(std::env::var_os("DATABASE_URL").is_none());
    assert!(std::env::var_os("RUST_LOG").is_none());

    // The real env var beats `.env`.
    std::env::set_var("RUST_LOG", "debug");
    assert_eq!(main_env_filter().to_string(), filter_str("debug"));
    std::env::remove_var("RUST_LOG");

    // A `.env` only in a parent directory is ignored.
    let child = tmp.path().join("child");
    std::fs::create_dir(&child).expect("mkdir");
    std::env::set_current_dir(&child).expect("chdir child");
    assert_eq!(main_env_filter().to_string(), filter_str("info,sqlx=warn"));

    // A bad line drops the file's value instead of aborting.
    std::fs::write(child.join(".env"), "RUST_LOG=error\nnot a valid line\n").expect("write");
    assert_eq!(main_env_filter().to_string(), filter_str("info,sqlx=warn"));
}
