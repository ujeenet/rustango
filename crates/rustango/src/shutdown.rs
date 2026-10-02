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

/// How long open connections get after the stop signal, when
/// `server.shutdown_timeout_secs` is unset (#1883).
pub const DEFAULT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The signal a graceful server stops accepting on.
pub type StopSignal = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Serve until [`shutdown_signal`], then give open connections `drain`
/// to finish. SSE and long-poll never finish on their own, so without a
/// deadline SIGTERM waited for SIGKILL (#1883).
///
/// ```ignore
/// serve_until_drained(|stop| axum::serve(listener, app).with_graceful_shutdown(stop), drain).await?;
/// ```
///
/// # Errors
/// What the server returns.
pub async fn serve_until_drained<F, S>(serve: F, drain: std::time::Duration) -> std::io::Result<()>
where
    F: FnOnce(StopSignal) -> S,
    S: std::future::IntoFuture<Output = std::io::Result<()>>,
{
    drain_on(Box::pin(shutdown_signal()), serve, drain).await
}

async fn drain_on<F, S>(
    signal: StopSignal,
    serve: F,
    drain: std::time::Duration,
) -> std::io::Result<()>
where
    F: FnOnce(StopSignal) -> S,
    S: std::future::IntoFuture<Output = std::io::Result<()>>,
{
    let (stopped, on_stop) = tokio::sync::oneshot::channel::<()>();
    let serve = serve(Box::pin(async move {
        signal.await;
        let _ = stopped.send(());
    }))
    .into_future();
    tokio::pin!(serve);
    tokio::select! {
        r = &mut serve => r,
        () = async {
            // A dropped sender means the server ended first; that arm wins.
            if on_stop.await.is_err() {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(drain).await;
        } => {
            tracing::warn!(
                target: "rustango::shutdown",
                drain_secs = drain.as_secs(),
                "drain deadline passed; closing the connections still open",
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that never ends (long-poll, SSE) must not hold
    /// shutdown past the deadline (#1883).
    #[tokio::test]
    async fn an_endless_request_does_not_outlive_the_drain_deadline() {
        use tokio::io::AsyncWriteExt as _;
        let app = axum::Router::new().route(
            "/poll",
            axum::routing::get(|| async {
                std::future::pending::<()>().await;
                ""
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, on_stop) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(drain_on(
            Box::pin(async move {
                let _ = on_stop.await;
            }),
            move |sig| axum::serve(listener, app).with_graceful_shutdown(sig),
            std::time::Duration::from_millis(200),
        ));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /poll HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        stop.send(()).unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
        assert!(
            done.is_ok(),
            "shutdown waited on the open request past its deadline"
        );
        drop(client);
    }
}
