//! SIGTERM stops the server *and* lets the code after it run (#1409).
//!
//! `tokio::signal::ctrl_c()` is **SIGINT only** on Unix. Every serve
//! path waited on it or on nothing, and SIGTERM's default disposition
//! kills the process outright — so the drain the docs promised never
//! ran under an orchestrator.
//!
//! SIGTERM is how everything asks a process to stop: Kubernetes before
//! its grace period, `docker stop`, systemd, most supervisors. Ctrl-C is
//! a developer's laptop. So the drain worked in exactly the environment
//! where losing a job does not matter and never in the one where it
//! does — and invisibly: exit 0, nothing logged, the work simply gone.
//!
//! ## Why this asserts the whole chain
//!
//! The first fix pinned only that `shutdown_signal()` resolves, and
//! that half worked. The half that was broken — serve returning so the
//! next line runs — had no test, and shipped broken through two serve
//! paths. A test covering the easy half of a two-part mechanism is a
//! test of the part that was never in doubt.
//!
//! So: real listener, real `axum::serve(..).with_graceful_shutdown(..)`,
//! real SIGTERM, and an assertion that the statement *after* the serve
//! executed. That last flag is the thing `Cli::on_shutdown` depends on.
//!
//! One test per binary on purpose — SIGTERM is process-wide, so a second
//! signalling test here would race this one.

#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn sigterm_drains_the_server_and_the_next_line_runs() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));

    // Set only after `serve` returns. Under the bug it stays false:
    // the process is killed mid-serve and never reaches the store.
    let after_serve = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&after_serve);

    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(rustango::shutdown::shutdown_signal())
            .await
            .expect("serve");
        flag.store(true, Ordering::SeqCst);
    });

    // Let the handler install before raising the signal; otherwise
    // SIGTERM hits the default disposition and kills the test binary
    // outright — which is the very behaviour being fixed. If this test
    // ever dies with signal 15 instead of failing an assertion, that IS
    // the regression.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(std::process::id().to_string())
        .status()
        .expect("spawn kill");
    assert!(status.success(), "could not signal our own process");

    let finished = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(
        finished.is_ok(),
        "the server must stop on SIGTERM — it waited on ctrl_c(), which is \
         SIGINT-only, so every orchestrator killed the process instead"
    );
    assert!(
        after_serve.load(Ordering::SeqCst),
        "the statement after `serve` must run — this is what makes \
         `Cli::on_shutdown` (and the documented `queue.shutdown()`) reachable \
         at all, and it is the half that shipped broken"
    );
}
