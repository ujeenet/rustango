//! Browser auto-reload — refreshes the page when the server restarts.
//!
//! Pairs with `cargo watch -x run` or `bacon run` for the canonical Rust
//! dev-loop. The watcher recompiles + restarts; this middleware injects
//! a `<script>` into HTML responses that polls a version endpoint. On
//! restart the version changes, browser reloads.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::livereload::{LiveReloadLayer, LiveReloadRouterExt, livereload_router};
//!
//! let app = Router::new()
//!     .route("/", get(index))
//!     .merge(livereload_router())            // serves /__livereload__/check
//!     .livereload(LiveReloadLayer::dev());   // injects <script> into HTML
//! ```
//!
//! In another terminal:
//!
//! ```bash
//! cargo install bacon
//! bacon run                                  # rebuilds + restarts on save
//! ```
//!
//! Browser opens to `http://localhost:8080`. Edit any source file → bacon
//! rebuilds → server restarts → browser reloads automatically.
//!
//! ## How it works
//!
//! 1. At startup, the layer captures a random session ID (process lifetime).
//! 2. Every HTML response gets a `<script>` injected before `</body>`.
//! 3. The script polls `/__livereload__/check` every poll_ms.
//! 4. The endpoint returns the session ID. The script remembers the first
//!    one it saw; if a later poll returns a different ID (= server restarted),
//!    `location.reload()`.
//!
//! ## Production warning
//!
//! **DO NOT use in production.** The injected script + version endpoint
//! make HTML pages slightly larger and add a polling request per second.
//! Gate the mount on `RUSTANGO_ENV != "prod"`:
//!
//! ```ignore
//! let app = Router::new().route(...);
//! let app = if std::env::var("RUSTANGO_ENV").as_deref() != Ok("prod") {
//!     app.merge(livereload_router()).livereload(LiveReloadLayer::dev())
//! } else {
//!     app
//! };
//! ```

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::header::{HeaderValue, CONTENT_TYPE};
use axum::http::Response;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

const ENDPOINT: &str = "/__livereload__/check";
const SESSION_HEADER: &str = "X-LiveReload-Session";

/// Layer configuration.
#[derive(Clone)]
pub struct LiveReloadLayer {
    session: String,
    poll_ms: u32,
    inject_into_html: bool,
}

impl LiveReloadLayer {
    /// Default dev config — 1-second poll, inject into HTML, fresh session.
    #[must_use]
    pub fn dev() -> Self {
        Self {
            session: random_session(),
            poll_ms: 1000,
            inject_into_html: true,
        }
    }

    /// New layer with a custom poll interval (ms).
    #[must_use]
    pub fn with_poll_ms(mut self, ms: u32) -> Self {
        self.poll_ms = ms.max(100); // floor at 100ms to avoid silly rates
        self
    }

    /// Disable HTML injection — the version endpoint still works, but the
    /// browser script isn't auto-installed (use this if you inject the
    /// script manually into your templates).
    #[must_use]
    pub fn without_injection(mut self) -> Self {
        self.inject_into_html = false;
        self
    }

    /// The session ID this layer was constructed with — exposed for tests
    /// and the version endpoint.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }
}

impl Default for LiveReloadLayer {
    fn default() -> Self {
        Self::dev()
    }
}

/// Extension trait — `.livereload(layer)` on Router.
pub trait LiveReloadRouterExt {
    #[must_use]
    fn livereload(self, layer: LiveReloadLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> LiveReloadRouterExt for Router<S> {
    fn livereload(self, layer: LiveReloadLayer) -> Self {
        let cfg = Arc::new(layer);
        let cfg_for_middleware = cfg.clone();
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg_for_middleware.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

#[derive(Clone, Default)]
struct EndpointSession {
    session: String,
}

/// Build the version endpoint router. Mount alongside your app routes.
#[must_use]
pub fn livereload_router() -> Router {
    livereload_router_with(LiveReloadLayer::dev())
}

/// Build the version endpoint router with an explicit layer (so the session
/// matches the injected script).
#[must_use]
pub fn livereload_router_with(layer: LiveReloadLayer) -> Router {
    let session = layer.session.clone();
    Router::new()
        .route(ENDPOINT, get(check))
        .layer(axum::Extension(EndpointSession { session }))
}

async fn check(
    axum::Extension(es): axum::Extension<EndpointSession>,
) -> impl IntoResponse {
    ([(SESSION_HEADER, HeaderValue::from_str(&es.session).unwrap_or(HeaderValue::from_static("?")))], es.session.clone())
}

async fn handle(
    cfg: Arc<LiveReloadLayer>,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let response = next.run(req).await;

    if !cfg.inject_into_html {
        return response;
    }

    // Only inject into successful HTML responses
    if !response.status().is_success() {
        return response;
    }
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |ct| ct.starts_with("text/html"));
    if !is_html {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };

    let html = String::from_utf8_lossy(&bytes);
    let injected = inject_script(&html, &cfg.session, cfg.poll_ms);
    let mut response = Response::from_parts(parts, Body::from(injected));
    // Recompute Content-Length is handled by axum implicitly when no header was set;
    // explicit override needed for old clients
    response.headers_mut().remove(axum::http::header::CONTENT_LENGTH);
    response
}

fn inject_script(html: &str, session: &str, poll_ms: u32) -> String {
    let script = format!(
        r#"<script>(function(){{var s="{session}";function p(){{fetch("{ENDPOINT}").then(r=>r.text()).then(v=>{{if(v.trim()&&v.trim()!==s)location.reload();}}).catch(()=>{{}});}}setInterval(p,{poll_ms});}})();</script>"#,
    );
    if let Some(idx) = html.rfind("</body>") {
        let mut out = String::with_capacity(html.len() + script.len());
        out.push_str(&html[..idx]);
        out.push_str(&script);
        out.push_str(&html[idx..]);
        out
    } else {
        // No </body> — append at the end
        format!("{html}{script}")
    }
}

fn random_session() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_session_unique() {
        let a = random_session();
        let b = random_session();
        assert_ne!(a, b);
        assert_eq!(a.len(), 11); // 8 bytes base64-no-pad
    }

    #[test]
    fn dev_preset_uses_1s_poll() {
        let l = LiveReloadLayer::dev();
        assert_eq!(l.poll_ms, 1000);
        assert!(l.inject_into_html);
    }

    #[test]
    fn poll_ms_floor_at_100() {
        let l = LiveReloadLayer::dev().with_poll_ms(0);
        assert_eq!(l.poll_ms, 100);
        let l = LiveReloadLayer::dev().with_poll_ms(50);
        assert_eq!(l.poll_ms, 100);
    }

    #[test]
    fn without_injection_disables_script() {
        let l = LiveReloadLayer::dev().without_injection();
        assert!(!l.inject_into_html);
    }

    #[test]
    fn inject_script_inserts_before_body_close() {
        let html = "<html><body>Hello</body></html>";
        let out = inject_script(html, "abc", 1000);
        assert!(out.contains("<script>"));
        let body_close = out.find("</body>").unwrap();
        let script_pos = out.find("<script>").unwrap();
        assert!(script_pos < body_close, "script must come before </body>");
        assert!(out.contains("\"abc\""));
        assert!(out.contains("1000"));
    }

    #[test]
    fn inject_script_appends_when_no_body_tag() {
        let html = "<p>fragment</p>";
        let out = inject_script(html, "x", 500);
        assert!(out.starts_with("<p>fragment</p>"));
        assert!(out.contains("<script>"));
    }

    #[test]
    fn inject_script_uses_endpoint_constant() {
        let out = inject_script("<body></body>", "x", 100);
        assert!(out.contains(ENDPOINT));
    }

    #[test]
    fn session_method_returns_value() {
        let l = LiveReloadLayer::dev();
        assert!(!l.session().is_empty());
    }
}
