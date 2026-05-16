//! Live HTTP server for tests — Django's `LiveServerTestCase`.
//!
//! Binds an `axum::Router` to a random localhost TCP port and runs
//! it on a background task; the helper handle exposes the base URL.
//! Use it when [`crate::test_client::TestClient`] (in-process
//! oneshot routing) isn't enough — typical cases:
//!
//! - Selenium / headless-browser tests
//! - End-to-end cookie + session round-trips against a real TCP socket
//! - WebSocket upgrades
//! - Code that explicitly reads `request.scheme()` / `host()`
//!
//! ```ignore
//! use rustango::test_server::LiveServer;
//!
//! #[tokio::test]
//! async fn home_returns_200_over_real_http() {
//!     let server = LiveServer::spawn(make_app()).await;
//!     let body = reqwest::get(server.url("/")).await.unwrap().text().await.unwrap();
//!     assert!(body.contains("Hello"));
//!     server.shutdown().await;
//! }
//! ```
//!
//! ## Lifetime
//!
//! `LiveServer::spawn` returns a handle that owns the listener +
//! background task. Drop the handle (or call `shutdown`) to stop
//! the server. The OS reclaims the bound port on drop too — the
//! port choice is `127.0.0.1:0` so each test gets its own port and
//! parallel tests don't collide.
//!
//! Issue #39 partial — Django's four-tier `TestCase` hierarchy.

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Live HTTP server bound to a random localhost port.
///
/// Hold onto the value; dropping it stops the server (the
/// background task observes the dropped shutdown sender and
/// exits). Prefer explicit [`Self::shutdown`] in tests so the
/// server can flush cleanly before the test function returns.
pub struct LiveServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl LiveServer {
    /// Bind `router` to a random `127.0.0.1` port and start serving
    /// in the background. Returns once the listener is accepting
    /// connections — tests can issue the first request immediately
    /// without a race against startup.
    ///
    /// # Panics
    /// On TCP-bind failure (port exhaustion, permission denied).
    /// Tests would otherwise observe a confusing connection refused
    /// from later requests; failing here surfaces the real reason.
    pub async fn spawn(router: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("LiveServer: bind 127.0.0.1:0");
        let addr = listener
            .local_addr()
            .expect("LiveServer: listener.local_addr()");
        let (tx, rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service())
                .with_graceful_shutdown(async move {
                    // Either shutdown signal arrives or the sender
                    // gets dropped — either way exit.
                    let _ = rx.await;
                })
                .await
                .ok();
        });
        Self {
            addr,
            shutdown: Some(tx),
            join: Some(join),
        }
    }

    /// Bound socket address (host:port). `127.0.0.1:<random>`.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base URL with the bound address, e.g. `http://127.0.0.1:54321`.
    /// No trailing slash; pass paths to [`Self::url`] for the
    /// concatenated form.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Build an absolute URL for `path`. Idempotent on leading slash.
    ///
    /// ```ignore
    /// server.url("/users/1")  // "http://127.0.0.1:54321/users/1"
    /// server.url("users/1")   // "http://127.0.0.1:54321/users/1"
    /// ```
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        let path = path.strip_prefix('/').unwrap_or(path);
        format!("http://{}/{}", self.addr, path)
    }

    /// Stop the server. Sends a shutdown signal to the background
    /// task and awaits its completion. Subsequent `shutdown` calls
    /// are no-ops.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        // Best-effort: fire the shutdown signal so the background
        // task exits even if the test forgot to call `shutdown`. We
        // can't await the JoinHandle here (Drop is sync), so the
        // task may outlive the LiveServer briefly. For deterministic
        // shutdown, call `shutdown()` explicitly.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    fn echo_app() -> Router {
        Router::new()
            .route("/", get(|| async { "hello world" }))
            .route(
                "/status",
                get(|| async { (axum::http::StatusCode::CREATED, "made") }),
            )
    }

    #[tokio::test]
    async fn spawn_returns_addr_on_loopback() {
        let server = LiveServer::spawn(echo_app()).await;
        let addr = server.addr();
        assert!(addr.ip().is_loopback(), "addr: {addr}");
        assert!(addr.port() > 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn base_url_shape() {
        let server = LiveServer::spawn(echo_app()).await;
        let url = server.base_url();
        assert!(url.starts_with("http://127.0.0.1:"), "url: {url}");
        // No trailing slash on the base.
        assert!(!url.ends_with('/'));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn url_handles_with_and_without_leading_slash() {
        let server = LiveServer::spawn(echo_app()).await;
        let a = server.url("/foo");
        let b = server.url("foo");
        assert_eq!(a, b, "leading slash should be normalised");
        assert!(a.ends_with("/foo"));
        server.shutdown().await;
    }

    /// End-to-end: spawn the server, hit it via a hand-rolled
    /// `TcpStream` HTTP/1.1 request, parse the response status +
    /// body. Avoids pulling reqwest just to verify the server
    /// accepts connections.
    #[tokio::test]
    async fn serves_get_root_over_real_tcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let server = LiveServer::spawn(echo_app()).await;
        let mut stream = TcpStream::connect(server.addr()).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let raw = String::from_utf8_lossy(&buf);
        assert!(raw.contains("HTTP/1.1 200"), "status line: {raw}");
        assert!(raw.contains("hello world"), "body: {raw}");
        server.shutdown().await;
    }

    /// Two servers in parallel get distinct ports — `127.0.0.1:0`
    /// asks the OS for a fresh port each time, so parallel tests
    /// don't collide.
    #[tokio::test]
    async fn parallel_servers_get_distinct_ports() {
        let a = LiveServer::spawn(echo_app()).await;
        let b = LiveServer::spawn(echo_app()).await;
        assert_ne!(a.addr().port(), b.addr().port());
        a.shutdown().await;
        b.shutdown().await;
    }
}
