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

/// Bind `addr` and log where the server listens: every serve path binds
/// here, so none starts silently. `apex` adds the operator console URL.
///
/// # Errors
/// The bind failure.
#[cfg_attr(not(any(feature = "manage", feature = "tenancy")), allow(dead_code))]
#[cfg(feature = "_axum")]
pub(crate) async fn bind_announced(
    addr: &str,
    apex: Option<&str>,
) -> std::io::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    match apex {
        Some(apex) => tracing::info!(
            target: "rustango::server",
            addr = %bound,
            "listening on http://{bound}; operator console at http://{apex}:{}/",
            bound.port()
        ),
        None => {
            tracing::info!(target: "rustango::server", addr = %bound, "listening on http://{bound}");
        }
    }
    Ok(listener)
}

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

/// Per-connection time limits of the serve loop.
#[cfg(feature = "_axum")]
#[derive(Debug, Clone, Copy)]
struct ConnLimits {
    /// A new connection must send its first request head in this time.
    first_head: std::time::Duration,
    /// Idle time plus head of every later request. Must exceed the
    /// proxy's idle timeout (ALB and nginx use 60 s), or it sees 502s.
    keep_alive: std::time::Duration,
}

#[cfg(feature = "_axum")]
impl ConnLimits {
    /// `RUSTANGO_HEADER_READ_TIMEOUT_SECS` (30) and `RUSTANGO_KEEP_ALIVE_TIMEOUT_SECS` (75).
    fn from_env() -> Self {
        Self {
            first_head: env_secs("RUSTANGO_HEADER_READ_TIMEOUT_SECS", 30),
            keep_alive: env_secs("RUSTANGO_KEEP_ALIVE_TIMEOUT_SECS", 75),
        }
    }
}

#[cfg(feature = "_axum")]
fn env_secs(name: &str, default: u64) -> std::time::Duration {
    let secs = match std::env::var(name) {
        Err(_) => default,
        Ok(v) => v.trim().parse::<u64>().ok().filter(|n| *n > 0).unwrap_or_else(|| {
            tracing::warn!(target: "rustango::shutdown", var = name, value = ?v, default, "not a positive number of seconds; using the default");
            default
        }),
    };
    std::time::Duration::from_secs(secs)
}

#[cfg(feature = "_axum")]
async fn drain_on(
    signal: StopSignal,
    listener: tokio::net::TcpListener,
    app: axum::Router,
    drain: std::time::Duration,
) -> std::io::Result<()> {
    serve_with(signal, listener, app, drain, ConnLimits::from_env()).await
}

/// HTTP/1, plus h2c with the `http2` feature. hyper's header timeout
/// also runs while idle, so it carries the keep-alive limit.
#[cfg(feature = "_axum")]
fn conn_builder(
    limits: ConnLimits,
) -> hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> {
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(limits.keep_alive);
    #[cfg(feature = "http2")]
    builder.http2().enable_connect_protocol();
    builder
}

/// `axum::serve` gives hyper no timer, which turns its header timeout
/// off; a slow client then held a connection forever.
#[cfg(feature = "_axum")]
async fn serve_with(
    mut signal: StopSignal,
    mut listener: tokio::net::TcpListener,
    app: axum::Router,
    drain: std::time::Duration,
    limits: ConnLimits,
) -> std::io::Result<()> {
    use hyper_util::rt::TokioIo;
    use tower::ServiceExt as _;

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
    // Each connection holds a receiver; all dropped means all closed.
    let (open_tx, open_rx) = tokio::sync::watch::channel(());
    loop {
        // `Listener::accept` retries accept errors (EMFILE) like `axum::serve`.
        let (io, addr) = tokio::select! {
            conn = axum::serve::Listener::accept(&mut listener) => conn,
            () = &mut signal => break,
        };
        let served = std::sync::Arc::new(tokio::sync::Notify::new());
        let on_request = std::sync::Arc::clone(&served);
        // `ConnectInfo` feeds `access_log`, IP filters and per-IP limits.
        let svc =
            app.clone()
                .map_request(move |mut req: axum::http::Request<hyper::body::Incoming>| {
                    req.extensions_mut()
                        .insert(axum::extract::ConnectInfo(addr));
                    on_request.notify_one();
                    req
                });
        let mut stop = stop_rx.clone();
        let open = open_rx.clone();
        tokio::spawn(async move {
            let builder = conn_builder(limits);
            let conn = builder.serve_connection_with_upgrades(
                TokioIo::new(io),
                hyper_util::service::TowerToHyperService::new(svc),
            );
            tokio::pin!(conn);
            // Slowloris guard for the first head; hyper times the later ones.
            let first_head = tokio::time::sleep(limits.first_head);
            tokio::pin!(first_head);
            let mut got_first = false;
            loop {
                tokio::select! {
                    biased;
                    _ = conn.as_mut() => break,
                    Ok(()) = stop.changed() => conn.as_mut().graceful_shutdown(),
                    () = served.notified(), if !got_first => got_first = true,
                    () = &mut first_head, if !got_first => break,
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

    /// A developer sees where the server listens, and the console URL with tenancy.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn binding_logs_the_address() {
        use tracing::instrument::WithSubscriber as _;
        for apex in [None, Some("shop.localhost")] {
            let out = crate::testkit::CaptureWriter::default();
            let sub = tracing_subscriber::fmt()
                .with_writer(out.clone())
                .with_ansi(false)
                .finish();
            let listener = bind_announced("127.0.0.1:0", apex)
                .with_subscriber(sub)
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let logs = out.contents();
            assert!(logs.contains(" INFO "), "{logs}");
            assert!(logs.contains(&format!("http://127.0.0.1:{port}")), "{logs}");
            if let Some(apex) = apex {
                assert!(logs.contains(&format!("http://{apex}:{port}/")), "{logs}");
            }
        }
    }

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

    /// Serve `/` with a 300 ms first-head and 1.5 s keep-alive limit; the stop sender keeps it up.
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
            ConnLimits {
                first_head: std::time::Duration::from_millis(300),
                keep_alive: std::time::Duration::from_millis(1500),
            },
        ));
        (addr, stop)
    }

    /// `drain_on` serving `app` until the returned sender fires.
    async fn spawn_drained(
        app: axum::Router,
        drain: std::time::Duration,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, on_stop) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(drain_on(
            Box::pin(async move {
                let _ = on_stop.await;
            }),
            listener,
            app,
            drain,
        ));
        (addr, stop, server)
    }

    /// Send one request, return the whole response text.
    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n");
        client.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        out
    }

    /// The handler sees the peer address through `ConnectInfo`.
    #[tokio::test]
    async fn the_handler_gets_the_peer_address() {
        use axum::extract::ConnectInfo;
        let app =
            axum::Router::new().route(
                "/ip",
                axum::routing::get(
                    |ConnectInfo(a): ConnectInfo<std::net::SocketAddr>| async move {
                        a.ip().to_string()
                    },
                ),
            );
        let (addr, _stop, _server) = spawn_drained(app, std::time::Duration::from_secs(1)).await;
        let resp = get(addr, "/ip").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.ends_with("127.0.0.1"), "{resp}");
    }

    /// A request running at the stop signal finishes; then the server returns.
    #[tokio::test]
    async fn an_in_flight_request_finishes_after_the_stop_signal() {
        let app = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                "done"
            }),
        );
        let (addr, stop, server) = spawn_drained(app, std::time::Duration::from_secs(10)).await;
        let client = tokio::spawn(async move { get(addr, "/slow").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        stop.send(()).unwrap();
        let resp = client.await.unwrap();
        assert!(
            resp.starts_with("HTTP/1.1 200") && resp.ends_with("done"),
            "{resp}"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server returned once the request was done")
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    /// With nothing open, shutdown does not wait for the drain deadline.
    #[tokio::test]
    async fn shutdown_with_no_connections_is_immediate() {
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let (addr, stop, server) = spawn_drained(app, std::time::Duration::from_secs(10)).await;
        assert!(get(addr, "/").await.starts_with("HTTP/1.1 200"));
        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("shutdown waited for the drain deadline")
            .unwrap()
            .unwrap();
    }

    /// An h2c prior-knowledge client gets HTTP/2 SETTINGS back.
    #[cfg(feature = "http2")]
    #[tokio::test]
    async fn h2c_prior_knowledge_is_served() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let (addr, _stop, _server) = spawn_drained(app, std::time::Duration::from_secs(1)).await;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Preface, then an empty SETTINGS frame.
        client
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
            .await
            .unwrap();
        let mut head = [0u8; 9];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_exact(&mut head),
        )
        .await
        .expect("no reply")
        .unwrap();
        assert_eq!(head[3], 0x4, "first frame is SETTINGS: {head:?}");
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

    /// A client that never finishes its headers is cut off.
    #[tokio::test]
    async fn a_slow_header_is_cut_off() {
        use tokio::io::AsyncWriteExt as _;
        let (addr, _stop) = slow_client_server().await;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n")
            .await
            .unwrap();
        assert!(
            closed_by_server(&mut client).await,
            "slow header held the connection"
        );
    }

    /// An idle keep-alive connection outlives the head limit and serves
    /// again, then closes at the keep-alive limit.
    #[tokio::test]
    async fn an_idle_keep_alive_connection_lives_to_its_own_limit() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (addr, _stop) = slow_client_server().await;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 256];
        for _ in 0..2 {
            client
                .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
                .await
                .unwrap();
            let n = client.read(&mut buf).await.unwrap();
            assert!(buf[..n].starts_with(b"HTTP/1.1 200"), "request is served");
            // Idle past the 300 ms first-head limit.
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        }
        assert!(
            closed_by_server(&mut client).await,
            "idle keep-alive stayed open"
        );
    }
}
