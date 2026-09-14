//! Graceful-shutdown signal handling.
//!
//! One implementation of "the process has been asked to stop", used by
//! every serve path so they cannot disagree (#1409).
//!
//! ## Why `ctrl_c()` alone is not enough
//!
//! `tokio::signal::ctrl_c()` is **SIGINT only** on Unix. It never
//! resolves for SIGTERM — and SIGTERM is how every orchestrator asks a
//! process to stop: Kubernetes before its grace period, `docker stop`,
//! systemd, most supervisors. SIGTERM's default disposition kills the
//! process outright, so a server waiting on `ctrl_c()` is simply gone.
//!
//! That put the drain in exactly the environment where losing work does
//! not matter — a developer's laptop — and never in the one where it
//! does. Every deploy and every pod eviction dropped whatever was in
//! flight, and the failure was invisible: exit 0, nothing logged.

/// Resolves when the process is asked to stop: Ctrl-C (SIGINT) or
/// SIGTERM.
///
/// On non-Unix targets only Ctrl-C exists, and that is what this waits
/// for.
///
/// Pass it to `axum::serve(..).with_graceful_shutdown(..)`, which then
/// stops accepting connections and lets in-flight requests finish.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        // A failed handler registration must not mean "never shut
        // down": fall back to the other signal rather than hanging.
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "rustango::shutdown",
                    error = %e,
                    "could not install a SIGTERM handler; Ctrl-C only",
                );
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!(target: "rustango::shutdown", signal = "SIGINT", "shutting down");
                return;
            }
        };

        let signal_name = tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        };
        tracing::info!(target: "rustango::shutdown", signal = signal_name, "shutting down");
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "rustango::shutdown", signal = "SIGINT", "shutting down");
    }
}
