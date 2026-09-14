//! Shutdown listens for SIGTERM, not just Ctrl-C (#1409).
//!
//! `tokio::signal::ctrl_c()` is **SIGINT only** on Unix. The tenancy
//! server waited on it and nothing else, and SIGTERM's default
//! disposition kills the process outright — so the graceful shutdown
//! that path already had never ran under an orchestrator.
//!
//! SIGTERM is how everything asks a process to stop: Kubernetes before
//! its grace period, `docker stop`, systemd, most supervisors. Ctrl-C is
//! a developer's laptop. So the drain worked in exactly the environment
//! where losing a job does not matter and never in the one where it
//! does — and invisibly: exit 0, nothing logged, the work simply gone.
//!
//! Asserting the signal half here rather than booting a server: the
//! future resolving on SIGTERM is the thing that was broken, and
//! `axum::serve(..).with_graceful_shutdown(..)` is axum's to test.

#![cfg(unix)]

use std::time::Duration;

/// The property: SIGTERM resolves the shutdown future.
///
/// Sends the signal to this process. With the old `ctrl_c()`-only
/// implementation the future never resolves and this times out — which
/// is exactly what a pod eviction did to an in-flight job.
#[tokio::test]
async fn sigterm_resolves_the_shutdown_future() {
    let shutdown = tokio::spawn(rustango::shutdown::shutdown_signal());

    // Give the handler a moment to install before raising the signal;
    // otherwise SIGTERM hits the default disposition and kills the test
    // binary outright — which is the very behaviour being fixed. (If
    // this test ever dies with signal 15 instead of failing an
    // assertion, that IS the regression.)
    tokio::time::sleep(Duration::from_millis(200)).await;
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(std::process::id().to_string())
        .status()
        .expect("spawn kill");
    assert!(status.success(), "could not signal our own process");

    let resolved = tokio::time::timeout(Duration::from_secs(5), shutdown).await;
    assert!(
        resolved.is_ok(),
        "shutdown_signal() must resolve on SIGTERM — it waited on ctrl_c(), \
         which is SIGINT-only, so every orchestrator killed the process \
         before anything drained"
    );
}
