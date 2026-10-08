//! `#[rustango::main]` loads `.env` before installing its subscriber,
//! so a `RUST_LOG` set only there sets the filter (#2204).
//!
//! Its own binary with one test: the global subscriber and the cwd are
//! process-wide.

#![cfg(feature = "runtime")]

use tracing::Level;

#[rustango::main]
async fn run_app() -> (bool, bool) {
    (
        tracing::enabled!(Level::INFO),
        tracing::enabled!(Level::ERROR),
    )
}

#[test]
fn rust_log_from_dotenv_reaches_the_installed_filter() {
    std::env::remove_var("RUST_LOG");
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(".env"), "RUST_LOG=error\n").expect("write .env");
    std::env::set_current_dir(tmp.path()).expect("chdir");

    let (info, error) = run_app();

    // The default filter is `info,sqlx=warn`; `info` off means `.env` won.
    assert!(!info, "RUST_LOG=error from .env must disable info events");
    assert!(error, "error events must stay enabled");
}
