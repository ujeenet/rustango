//! `Setup::with_file` rolls log events to a rotating file appender —
//! closes future-backlog item #1 ("advanced logging config:
//! multiple processors, JSON formatter, pluggable formatter/sink").
//!
//! Lives in its own test binary so the `try_init`-style global
//! subscriber installed below doesn't conflict with the in-crate
//! unit tests' subscribers.

#![cfg(feature = "runtime")]

use rustango::logging::{Rotation, Setup};
use std::time::Duration;

#[test]
fn with_file_writes_event_to_disk() {
    let dir = std::env::temp_dir().join(format!(
        "_rustango_log_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("create tempdir");

    let guard = Setup::new()
        .with_file(&dir, "test-app", Rotation::Never)
        .file_only()
        .with_default_env_filter("info")
        .install();

    tracing::info!("rustango_file_appender_marker");

    // Drop the guard to flush queued writes through the appender's
    // worker thread. Without this, the non-blocking writer can hold
    // events in its internal buffer.
    drop(guard);
    // The non-blocking writer flushes async; give the worker a beat
    // to land bytes on disk before we read.
    std::thread::sleep(Duration::from_millis(150));

    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read tempdir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("test-app"))
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "expected at least one log file under {dir:?}, found none",
    );

    let mut joined = String::new();
    for path in &entries {
        joined.push_str(&std::fs::read_to_string(path).expect("read log file"));
        joined.push('\n');
    }
    assert!(
        joined.contains("rustango_file_appender_marker"),
        "log file should contain the emitted marker: contents = {joined}",
    );

    let _ = std::fs::remove_dir_all(&dir);
}
