#![cfg(feature = "runtime")]
//! Live integration test for `logging::Setup::with_file` (v0.30.11).
//!
//! Lives in its own integration test file so it owns the global
//! subscriber (each `cargo test --test FILE` runs in a fresh
//! process). The unit tests in `src/logging.rs` cover the builder
//! API; this file verifies the actual end-to-end disk-write path:
//! event emitted → appender thread flushes on guard drop → file
//! exists at `<dir>/<prefix>.YYYY-MM-DD` with the event line.

use rustango::logging::{Rotation, Setup};

#[test]
fn with_file_actually_writes_to_disk() {
    let dir = std::env::temp_dir().join(format!("rustango_logging_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    {
        let _guard = Setup::new()
            .with_file(dir.clone(), "app", Rotation::Daily)
            .file_only() // skip stdout so test output isn't polluted
            .install()
            .expect("file sink installed");

        tracing::info!(target: "rustango_logging_smoke", "regression-test-marker-line");
        // Drop the guard → tracing-appender flushes the queued
        // writes synchronously, so when this scope ends the file
        // is on disk.
    }

    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("logging dir `{}` was never created: {e}", dir.display()))
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("app."))
        .collect::<Vec<_>>();
    assert!(
        !entries.is_empty(),
        "no rolling-file output in {} — expected at least one app.YYYY-MM-DD",
        dir.display()
    );

    let mut found_marker = false;
    for entry in entries {
        let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if body.contains("regression-test-marker-line") {
            found_marker = true;
            break;
        }
    }
    assert!(
        found_marker,
        "marker line missing from rolling file — appender never flushed?"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
