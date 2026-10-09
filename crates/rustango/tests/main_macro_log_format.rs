//! `#[rustango::main]` logs JSON when `RUSTANGO__LOGGING__FORMAT=json`
//! is in the env or in `./.env` (#2258).
//!
//! The subscriber is process-global, so each case re-runs this binary's
//! ignored `child_logs_one_event` test in a fresh process.

#![cfg(feature = "runtime")]

use std::process::Command;

const KEY: &str = "RUSTANGO__LOGGING__FORMAT";

#[rustango::main]
async fn log_marker() {
    tracing::info!("format-marker");
}

#[test]
#[ignore = "run by format_env_var_switches_main_to_json in a child process"]
fn child_logs_one_event() {
    log_marker();
}

/// The child's stdout line carrying the marker event.
fn marker_line(env: Option<&str>, dotenv: Option<&str>) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    if let Some(v) = dotenv {
        std::fs::write(tmp.path().join(".env"), format!("{KEY}={v}\n")).expect("write .env");
    }
    let mut cmd = Command::new(std::env::current_exe().expect("exe"));
    cmd.args([
        "child_logs_one_event",
        "--exact",
        "--ignored",
        "--nocapture",
    ])
    .current_dir(tmp.path())
    .env_remove(KEY)
    .env_remove("RUST_LOG")
    .env("NO_COLOR", "1");
    if let Some(v) = env {
        cmd.env(KEY, v);
    }
    let out = cmd.output().expect("run child");
    assert!(out.status.success(), "child failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("format-marker"))
        .unwrap_or_else(|| panic!("no marker in child output: {out:?}"))
        .to_owned()
}

fn is_json(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok()
}

#[test]
fn format_env_var_switches_main_to_json() {
    assert!(!is_json(&marker_line(None, None)), "default must stay full");
    assert!(is_json(&marker_line(Some("json"), None)), "env var ignored");
    assert!(
        is_json(&marker_line(None, Some("json"))),
        ".env value ignored"
    );
    assert!(
        !is_json(&marker_line(Some("full"), Some("json"))),
        "the real env must win over .env"
    );
}
