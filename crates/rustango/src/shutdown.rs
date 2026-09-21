//! Graceful-shutdown signal handling. Every serve path uses this
//! one function, so they cannot disagree.
//!
//! ## Why `ctrl_c()` alone is not enough
//!
//! On Unix `tokio::signal::ctrl_c()` waits for SIGINT only. But
//! Kubernetes, `docker stop` and systemd all send SIGTERM, which
//! kills the process by default. A server waiting on `ctrl_c()`
//! therefore drains on a laptop and dies mid-request in
//! production, with exit code 0 and nothing in the log.

/// Waits until the process is asked to stop, by Ctrl-C (SIGINT) or
/// SIGTERM. Off Unix, only Ctrl-C.
///
/// Pass it to `axum::serve(..).with_graceful_shutdown(..)`. The
/// server then stops accepting connections and lets the requests
/// already running finish.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        // If we cannot install the handler, fall back to Ctrl-C.
        // Hanging forever would be worse.
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
