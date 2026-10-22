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

/// Serve `app` until [`shutdown_signal`], then give open connections
/// `drain` to finish. SSE and long-poll never finish on their own, so
/// without a deadline SIGTERM waited for SIGKILL (#1883). This is the
/// crate's only `axum::serve`, so no serve path can skip the drain (#1948).
///
/// ```ignore
/// serve_until_drained(listener, app, drain).await?;
/// ```
///
/// # Errors
/// What the server returns.
#[cfg(feature = "_axum")]
pub async fn serve_until_drained(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    drain: std::time::Duration,
) -> std::io::Result<()> {
    drain_on(Box::pin(shutdown_signal()), listener, app, drain).await
}

/// Time a client gets to send each request head, and the most an idle
/// keep-alive connection stays open (#1211). hyper's own default.
#[cfg(feature = "_axum")]
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(feature = "_axum")]
async fn drain_on(
    signal: StopSignal,
    listener: tokio::net::TcpListener,
    app: axum::Router,
    drain: std::time::Duration,
) -> std::io::Result<()> {
    serve_with(signal, listener, app, drain, HEADER_READ_TIMEOUT).await
}

/// `axum::serve` gives hyper no timer, which turns its header timeout
/// off; a slow client then held a connection forever (#1211).
#[cfg(feature = "_axum")]
async fn serve_with(
    mut signal: StopSignal,
    mut listener: tokio::net::TcpListener,
    app: axum::Router,
    drain: std::time::Duration,
    header_read: std::time::Duration,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioIo, TokioTimer};
    use tower::ServiceExt as _;

    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
    // Each connection holds a receiver; all dropped means all closed.
    let (open_tx, open_rx) = tokio::sync::watch::channel(());
    loop {
        // `Listener::accept` retries accept errors (EMFILE) like `axum::serve`.
        let (io, addr) = tokio::select! {
            conn = axum::serve::Listener::accept(&mut listener) => conn,
            () = &mut signal => break,
        };
        // `ConnectInfo` feeds `access_log`, IP filters and per-IP limits.
        let svc = app.clone().map_request(
            move |mut req: axum::http::Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(axum::extract::ConnectInfo(addr));
                req
            },
        );
        let conn = builder
            .serve_connection(
                TokioIo::new(io),
                hyper_util::service::TowerToHyperService::new(svc),
            )
            .with_upgrades();
        let mut stop = stop_rx.clone();
        let open = open_rx.clone();
        tokio::spawn(async move {
            tokio::pin!(conn);
            loop {
                tokio::select! {
                    _ = conn.as_mut() => break,
                    Ok(()) = stop.changed() => conn.as_mut().graceful_shutdown(),
                }
            }
            drop(open);
        });
    }
    drop(listener);
    drop(open_rx);
    let _ = stop_tx.send(());
    tokio::select! {
        () = open_tx.closed() => {}
        () = tokio::time::sleep(drain) => {
            tracing::warn!(
                target: "rustango::shutdown",
                drain_secs = drain.as_secs(),
                "drain deadline passed; closing the connections still open",
            );
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "_axum"))]
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
            listener,
            app,
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

    /// Serve `/` with a 300 ms header timeout; the stop sender keeps it up.
    async fn slow_client_server() -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, on_stop) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve_with(
            Box::pin(async move {
                let _ = on_stop.await;
            }),
            listener,
            app,
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(300),
        ));
        (addr, stop)
    }

    /// Reads until the server closes; `false` if it is still open at 3 s.
    async fn closed_by_server(client: &mut tokio::net::TcpStream) -> bool {
        use tokio::io::AsyncReadExt as _;
        let mut buf = [0u8; 1024];
        let read_to_end = async {
            loop {
                match client.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), read_to_end)
            .await
            .is_ok()
    }

    /// A client that never finishes its headers is cut off (#1211).
    #[tokio::test]
    async fn a_slow_header_is_cut_off() {
        use tokio::io::AsyncWriteExt as _;
        let (addr, _stop) = slow_client_server().await;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n").await.unwrap();
        assert!(closed_by_server(&mut client).await, "slow header held the connection");
    }

    /// An idle keep-alive connection is closed too (#1211).
    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_closed() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (addr, _stop) = slow_client_server().await;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        assert!(buf[..n].starts_with(b"HTTP/1.1 200"), "first request is served");
        assert!(closed_by_server(&mut client).await, "idle keep-alive stayed open");
    }
}
