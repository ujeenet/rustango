//! A real HTTP server for tests, like Django's
//! `LiveServerTestCase`.
//!
//! It binds an `axum::Router` to a random localhost port and serves
//! it on a background task. Reach for it when
//! [`crate::test_client::TestClient`] is not enough, for example
//! with a headless browser, WebSocket upgrades, or code that reads
//! the request scheme or host.
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
//! The handle owns the listener and the background task. Drop it,
//! or call `shutdown`, to stop the server. Each server binds
//! `127.0.0.1:0`, so parallel tests never share a port.

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// An HTTP server on a random localhost port.
///
/// Keep the value alive; dropping it stops the server. In tests
/// call [`Self::shutdown`] instead, so the server finishes before
/// the test returns.
pub struct LiveServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl LiveServer {
    /// Bind `router` to a random `127.0.0.1` port and serve it in
    /// the background. It returns once the listener accepts
    /// connections, so the first request cannot race startup.
    ///
    /// # Panics
    /// If the TCP bind fails. Panicking here names the real cause,
    /// instead of leaving later requests to fail with "connection
    /// refused".
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
                    // Exit on the signal, or when the sender drops.
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

    /// The bound address, `127.0.0.1:<random port>`.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base URL, such as `http://127.0.0.1:54321`. No trailing
    /// slash; use [`Self::url`] to add a path.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Absolute URL for `path`. A leading slash is optional.
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

    /// Stop the server and wait for the background task to finish.
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
        // Signal the task even if the test forgot to call
        // `shutdown`. Drop is sync, so we cannot await the task and
        // it may outlive this handle for a moment.
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

    /// Talks HTTP/1.1 over a raw `TcpStream`, so the test does not
    /// need an HTTP client crate.
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

    /// `127.0.0.1:0` asks the OS for a fresh port each time, so
    /// parallel tests do not collide.
    #[tokio::test]
    async fn parallel_servers_get_distinct_ports() {
        let a = LiveServer::spawn(echo_app()).await;
        let b = LiveServer::spawn(echo_app()).await;
        assert_ne!(a.addr().port(), b.addr().port());
        a.shutdown().await;
        b.shutdown().await;
    }
}
