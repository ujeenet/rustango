//! Outbound webhooks: POST an HMAC-signed JSON payload to a subscriber
//! URL through the background job queue.
//!
//! Joins [`crate::webhook`] signing to the [`crate::jobs`] retry and
//! backoff machinery behind one call:
//!
//! ```ignore
//! use rustango::webhook::SignatureFormat;
//! use rustango::webhook_delivery::WebhookSubscription;
//!
//! // Once per app — register the delivery handler.
//! WebhookSubscription::register(&queue).await;
//!
//! // Per outbound event:
//! WebhookSubscription::new(
//!         "https://customer.example.com/hooks",
//!         "shared-secret-32-bytes",
//!     )
//!     .signature_format(SignatureFormat::HexSha256WithPrefix)
//!     .header("X-Tenant-Id", "acme")
//!     .dispatch(&queue, "order.created", &serde_json::json!({"order_id": 42}))
//!     .await?;
//! ```
//!
//! ## What gets sent
//!
//! - `POST <target_url>`
//! - Body: the payload as JSON, serialized and signed once at dispatch.
//! - `Content-Type: application/json`
//! - `User-Agent: rustango-webhook/<crate version>`
//! - `X-Webhook-Id: <uuid>`, the same on every retry so the receiver
//!   can drop duplicates.
//! - `X-Webhook-Event: <event_name>`
//! - `X-Webhook-Signature: <signature>`, in your chosen
//!   [`SignatureFormat`].
//! - Any extra headers from [`WebhookSubscription::header`].
//!
//! ## Retry policy
//!
//! - 2xx: done.
//! - 408, 429 and 5xx: retried with backoff, up to `MAX_ATTEMPTS`.
//! - Other 4xx: dead-lettered at once. A bad URL or bad auth will not
//!   fix itself.
//! - Transport errors (refused connection, DNS, TLS): retried.
//!
//! Add more retryable codes with
//! [`WebhookSubscription::retry_status_codes`].
//!
//! ## Target checks
//!
//! Only `http` and `https`. Redirects are not followed. Every resolved
//! address must be public; loopback, private, link-local, CGNAT and
//! multicast targets are dead-lettered, and addresses are checked again
//! at connect time. [`WebhookSubscription::allow_private_targets`]
//! turns the address check off; the `RUSTANGO_OUTBOUND_ALLOW` list never
//! applies, since a tenant may set the URL.
//! Only the status code is kept on failure.
//!
//! The queued job keeps the target URL and extra headers, so a secret in
//! either is stored with it; the signing secret is not.
//!
//! [`SignatureFormat`]: crate::webhook::SignatureFormat
//! [`WebhookSubscription::header`]: crate::webhook_delivery::WebhookSubscription::header
//! [`WebhookSubscription::retry_status_codes`]: crate::webhook_delivery::WebhookSubscription::retry_status_codes
//! [`WebhookSubscription::allow_private_targets`]: crate::webhook_delivery::WebhookSubscription::allow_private_targets

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::jobs::{Job, JobError, JobQueue};
use crate::outbound::{self, TargetError, TargetPolicy};
use crate::webhook::{sign as sign_body, SignatureFormat};

/// Per-delivery UUID. Receivers can use it to drop duplicates.
pub const HEADER_ID: &str = "X-Webhook-Id";
/// Header that carries the event name.
pub const HEADER_EVENT: &str = "X-Webhook-Event";
/// Header that carries the HMAC signature of the body.
pub const HEADER_SIGNATURE: &str = "X-Webhook-Signature";

/// User-Agent advertised on every delivery.
pub static USER_AGENT: &str = concat!("rustango-webhook/", env!("CARGO_PKG_VERSION"));

/// One outbound webhook event: the [`Job`] payload the queue stores,
/// retries and finally delivers or dead-letters.
///
/// [`WebhookSubscription::dispatch`] builds these for you.
#[derive(Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub id: String,
    pub event: String,
    pub target_url: String,
    /// The JSON body, signed at dispatch: the stored job never holds
    /// the secret (#1852).
    pub body: String,
    /// `X-Webhook-Signature` over `body`.
    pub signature: String,
    pub headers: HashMap<String, String>,
    pub timeout_secs: u64,
    /// Extra status codes to retry, on top of 408, 429 and 5xx.
    pub retry_status_codes: Vec<u16>,
    /// Allow loopback, private and link-local targets. Off by default.
    #[serde(default)]
    pub allow_private_targets: bool,
}

/// Only the URL's origin and the header names: both can carry secrets (#2161).
impl std::fmt::Debug for WebhookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookEvent")
            .field("id", &self.id)
            .field("event", &self.event)
            .field("target_url", &url_origin(&self.target_url))
            .field("body", &self.body)
            .field("signature", &self.signature)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("timeout_secs", &self.timeout_secs)
            .field("retry_status_codes", &self.retry_status_codes)
            .field("allow_private_targets", &self.allow_private_targets)
            .finish()
    }
}

/// `scheme://host:port` of a target URL: its userinfo, path and query
/// can be the receiver's secret (#1852).
fn url_origin(url: &str) -> String {
    reqwest::Url::parse(url).map_or_else(
        |_| "<invalid url>".to_owned(),
        |u| u.origin().ascii_serialization(),
    )
}

#[async_trait::async_trait]
impl Job for WebhookEvent {
    const NAME: &'static str = "rustango.webhook_delivery";
    /// 8 attempts in total, so 7 retries. With the queue's
    /// `1s * 2^attempt` backoff that spans about two minutes. Not
    /// configurable: `WebhookSubscription` has no attempts setting.
    const MAX_ATTEMPTS: u32 = 8;

    async fn run(&self) -> Result<(), JobError> {
        deliver(self).await
    }
}

async fn deliver(event: &WebhookEvent) -> Result<(), JobError> {
    // Only the subscription opts in: the operator's allowlist is for SSO
    // and Slack, and a tenant may own this URL.
    let policy = if event.allow_private_targets {
        TargetPolicy::AllowPrivate
    } else {
        TargetPolicy::public_only()
    };
    let egress =
        outbound::shared(policy).map_err(|e| JobError::Queue(format!("build http client: {e}")))?;
    let target = egress.check(&event.target_url).await.map_err(|e| match e {
        TargetError::Dns(_) => JobError::Retryable(e.to_string()),
        _ => JobError::Fatal(e.to_string()),
    })?;

    let mut req = target
        .request(reqwest::Method::POST)
        .timeout(Duration::from_secs(event.timeout_secs.max(1)))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(HEADER_ID, &event.id)
        .header(HEADER_EVENT, &event.event)
        .header(HEADER_SIGNATURE, &event.signature)
        .body(event.body.clone());
    for (k, v) in &event.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    // A default, as the per-call client had: a subscription header wins.
    if !event
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("user-agent"))
    {
        req = req.header(reqwest::header::USER_AGENT, USER_AGENT);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Transport, DNS or TLS: worth retrying. No URL: its path
            // can be the receiver's secret (#1852).
            return Err(JobError::Retryable(format!(
                "transport: {}",
                e.without_url()
            )));
        }
    };
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    // Status only: the response body is never stored.
    let msg = format!("status {status}");
    if event.retry_status_codes.contains(&status) || is_default_retryable(status) {
        Err(JobError::Retryable(msg))
    } else {
        Err(JobError::Fatal(msg))
    }
}

fn is_default_retryable(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// Config for one webhook subscriber, plus methods to register the
/// delivery handler and send events.
///
/// Keep one per subscription. Cheap to clone.
#[derive(Clone)]
pub struct WebhookSubscription {
    target_url: String,
    secret: String,
    signature_format: SignatureFormat,
    headers: HashMap<String, String>,
    timeout: Duration,
    retry_status_codes: Vec<u16>,
    allow_private_targets: bool,
}

/// The secret, header values and URL path are redacted (#2116, #2161).
impl std::fmt::Debug for WebhookSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookSubscription")
            .field("target_url", &url_origin(&self.target_url))
            .field("secret", &"<redacted>")
            .field("signature_format", &self.signature_format)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("timeout", &self.timeout)
            .field("retry_status_codes", &self.retry_status_codes)
            .field("allow_private_targets", &self.allow_private_targets)
            .finish()
    }
}

impl WebhookSubscription {
    pub fn new(target_url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            target_url: target_url.into(),
            secret: secret.into(),
            signature_format: SignatureFormat::HexSha256WithPrefix,
            headers: HashMap::new(),
            timeout: Duration::from_secs(10),
            retry_status_codes: Vec::new(),
            allow_private_targets: false,
        }
    }

    #[must_use]
    pub fn signature_format(mut self, fmt: SignatureFormat) -> Self {
        self.signature_format = fmt;
        self
    }

    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    #[must_use]
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Retry these status codes too, on top of 408, 429 and 5xx.
    #[must_use]
    pub fn retry_status_codes(mut self, codes: impl IntoIterator<Item = u16>) -> Self {
        self.retry_status_codes.extend(codes);
        self
    }

    /// Allow delivery to loopback, private and link-local addresses, for
    /// tests and intranet receivers. Off by default.
    #[must_use]
    pub fn allow_private_targets(mut self, allow: bool) -> Self {
        self.allow_private_targets = allow;
        self
    }

    /// Register the delivery [`Job`] on `queue`. Call once at startup,
    /// before [`Self::dispatch`]. Safe to call twice.
    pub async fn register<Q: JobQueue>(queue: &Q) {
        queue.register::<WebhookEvent>().await;
    }

    /// Queue a [`WebhookEvent`] and return its id at once. A worker
    /// delivers it later.
    ///
    /// # Errors
    /// [`JobError::Queue`] if the enqueue fails: database down, channel
    /// closed, or a payload that will not serialize.
    pub async fn dispatch<Q: JobQueue>(
        &self,
        queue: &Q,
        event_name: impl Into<String>,
        payload: impl Serialize,
    ) -> Result<String, JobError> {
        let id = Uuid::new_v4().to_string();
        let body = serde_json::to_string(&payload)
            .map_err(|e| JobError::Queue(format!("payload serialize: {e}")))?;
        let event = WebhookEvent {
            id: id.clone(),
            event: event_name.into(),
            target_url: self.target_url.clone(),
            signature: sign_body(
                self.signature_format,
                self.secret.as_bytes(),
                body.as_bytes(),
            )
            .map_err(|e| JobError::Queue(e.to_string()))?,
            body,
            headers: self.headers.clone(),
            timeout_secs: self.timeout.as_secs().max(1),
            retry_status_codes: self.retry_status_codes.clone(),
            allow_private_targets: self.allow_private_targets,
        };
        queue.dispatch(&event).await?;
        Ok(id)
    }
}

// How apps usually share one subscription across handlers.
pub type SharedSubscription = Arc<WebhookSubscription>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::InMemoryJobQueue;
    use axum::routing::post;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    /// Start a tiny axum server on a random port. It records each
    /// request into `received` and replies with `respond_status`.
    async fn start_server(
        respond_status: u16,
        received: Arc<Mutex<Vec<(reqwest::StatusCode, HashMap<String, String>, Vec<u8>)>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        // Shared state so the handler can read the status code without
        // it changing the closure type.
        let status = Arc::new(std::sync::atomic::AtomicU16::new(respond_status));
        let status_clone = status.clone();
        let received_clone = received.clone();

        let app = Router::new().route(
            "/hook",
            post(move |req: axum::extract::Request| {
                let received = received_clone.clone();
                let status = status_clone.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let bytes = axum::body::to_bytes(body, 1 << 20)
                        .await
                        .unwrap_or_default();
                    let mut hdrs = HashMap::new();
                    for (k, v) in parts.headers.iter() {
                        if let Ok(s) = v.to_str() {
                            hdrs.insert(k.as_str().to_owned(), s.to_owned());
                        }
                    }
                    received.lock().unwrap().push((
                        reqwest::StatusCode::from_u16(status.load(Ordering::SeqCst))
                            .unwrap_or(reqwest::StatusCode::OK),
                        hdrs,
                        bytes.to_vec(),
                    ));
                    let s = status.load(Ordering::SeqCst);
                    axum::http::StatusCode::from_u16(s).unwrap_or(axum::http::StatusCode::OK)
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/hook");
        let h = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Give the server a moment to start accepting.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (url, h)
    }

    #[tokio::test]
    async fn deliver_success_2xx() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let (url, srv) = start_server(200, received.clone()).await;

        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;

        let id = WebhookSubscription::new(url, "secret-bytes")
            .allow_private_targets(true)
            .header("X-Tenant", "acme")
            .dispatch(&q, "order.created", &serde_json::json!({"order_id": 42}))
            .await
            .unwrap();

        // Wait briefly for delivery.
        for _ in 0..50 {
            if !received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recv = received.lock().unwrap();
        assert_eq!(recv.len(), 1, "expected one delivery");
        let (_status, hdrs, body) = &recv[0];
        assert_eq!(hdrs.get("x-webhook-id"), Some(&id));
        assert_eq!(
            hdrs.get("x-webhook-event").map(String::as_str),
            Some("order.created")
        );
        assert!(hdrs.get("x-webhook-signature").is_some());
        assert_eq!(hdrs.get("x-tenant").map(String::as_str), Some("acme"));
        let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(parsed["order_id"], 42);

        srv.abort();
        q.shutdown().await;
    }

    #[tokio::test]
    async fn signature_header_matches_format() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let (url, srv) = start_server(200, received.clone()).await;

        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;

        WebhookSubscription::new(url, "secret-bytes")
            .allow_private_targets(true)
            .signature_format(SignatureFormat::HexSha256WithPrefix)
            .dispatch(&q, "ping", &serde_json::json!({"x": 1}))
            .await
            .unwrap();

        for _ in 0..50 {
            if !received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recv = received.lock().unwrap();
        let sig = recv[0].1.get("x-webhook-signature").unwrap();
        assert!(
            sig.starts_with("sha256="),
            "HexSha256WithPrefix should produce sha256=… (got: {sig})"
        );

        srv.abort();
        q.shutdown().await;
    }

    #[tokio::test]
    async fn fatal_on_4xx_other_than_408_429() {
        // 404 is fatal: dead-letter at once, no retry.
        let received = Arc::new(Mutex::new(Vec::new()));
        let (url, srv) = start_server(404, received.clone()).await;

        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        let dl_count = Arc::new(AtomicUsize::new(0));
        let dl = dl_count.clone();
        q.on_dead_letter(move |_dl| {
            let dl = dl.clone();
            async move {
                dl.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        q.start().await;

        WebhookSubscription::new(url, "secret-bytes")
            .allow_private_targets(true)
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();

        // Wait for delivery and the dead-letter callback.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            received.lock().unwrap().len(),
            1,
            "tried once, then gave up"
        );
        assert_eq!(dl_count.load(Ordering::SeqCst), 1, "dead-letter fired");

        srv.abort();
        q.shutdown().await;
    }

    #[tokio::test]
    async fn retryable_on_5xx() {
        // Two 503s, then a 200. The queue backs off 1s, then 2s.
        let received = Arc::new(Mutex::new(Vec::new()));
        let status_seq = Arc::new(Mutex::new(vec![503u16, 503u16, 200u16]));

        let app = Router::new().route(
            "/hook",
            post({
                let received = received.clone();
                let status_seq = status_seq.clone();
                move |req: axum::extract::Request| {
                    let received = received.clone();
                    let status_seq = status_seq.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let bytes = axum::body::to_bytes(body, 1 << 20)
                            .await
                            .unwrap_or_default();
                        let mut hdrs = HashMap::new();
                        for (k, v) in parts.headers.iter() {
                            if let Ok(s) = v.to_str() {
                                hdrs.insert(k.as_str().to_owned(), s.to_owned());
                            }
                        }
                        let next_status = {
                            let mut q = status_seq.lock().unwrap();
                            if q.is_empty() {
                                200
                            } else {
                                q.remove(0)
                            }
                        };
                        received.lock().unwrap().push((
                            reqwest::StatusCode::from_u16(next_status)
                                .unwrap_or(reqwest::StatusCode::OK),
                            hdrs,
                            bytes.to_vec(),
                        ));
                        axum::http::StatusCode::from_u16(next_status)
                            .unwrap_or(axum::http::StatusCode::OK)
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;

        WebhookSubscription::new(url, "s")
            .allow_private_targets(true)
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();

        // 503, 1s backoff, 503, 2s backoff, 200. The sleep leaves room.
        tokio::time::sleep(Duration::from_millis(7500)).await;
        let recv = received.lock().unwrap();
        assert!(
            recv.len() >= 3,
            "expected at least 3 delivery attempts, got {}",
            recv.len()
        );

        srv.abort();
        q.shutdown().await;
    }

    #[test]
    fn default_retryable_classification() {
        assert!(is_default_retryable(408));
        assert!(is_default_retryable(429));
        assert!(is_default_retryable(500));
        assert!(is_default_retryable(503));
        assert!(is_default_retryable(599));
        assert!(!is_default_retryable(404));
        assert!(!is_default_retryable(401));
        assert!(!is_default_retryable(200));
        assert!(!is_default_retryable(301));
    }

    /// #1852 — the queued job carries a signature, never the secret.
    #[tokio::test]
    async fn the_queued_job_does_not_hold_the_secret() {
        const SECRET: &str = "the-shared-signing-secret";
        let received = Arc::new(Mutex::new(Vec::new()));
        let (url, srv) = start_server(404, received.clone()).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        let dead = Arc::new(Mutex::new(Vec::new()));
        let d = dead.clone();
        q.on_dead_letter(move |dl| {
            let d = d.clone();
            async move { d.lock().unwrap().push(dl.payload) }
        })
        .await;
        q.start().await;
        WebhookSubscription::new(url, SECRET)
            .allow_private_targets(true)
            .dispatch(&q, "ping", &serde_json::json!({"b": 1, "a": 2}))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while dead.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let stored = dead
            .lock()
            .unwrap()
            .first()
            .expect("dead-lettered")
            .to_string();
        assert!(!stored.contains(SECRET), "{stored}");
        let recv = received.lock().unwrap();
        let (_, hdrs, body) = &recv[0];
        assert!(crate::webhook::verify_signature(
            SignatureFormat::HexSha256WithPrefix,
            SECRET.as_bytes(),
            body,
            &hdrs["x-webhook-signature"],
        ));
        srv.abort();
        q.shutdown().await;
    }

    /// #2116 — `{:?}` in a log must not print the signing secret.
    #[test]
    fn debug_redacts_the_secret_and_header_values() {
        let sub = WebhookSubscription::new("https://example.com/hook", "the-signing-secret")
            .header("Authorization", "Bearer the-header-token");
        let dbg = format!("{sub:?}");
        assert!(!dbg.contains("the-signing-secret"), "{dbg}");
        assert!(!dbg.contains("the-header-token"), "{dbg}");
        assert!(dbg.contains("https://example.com") && dbg.contains("Authorization"));
    }

    /// #2161 — neither Debug prints the URL path or a header value.
    #[test]
    fn debug_redacts_the_url_path_and_event_header_values() {
        let url = "https://u:pw@hooks.example.com/services/PATHSECRET?token=QSECRET";
        let sub = WebhookSubscription::new(url, "s");
        let mut ev = event(url.to_owned(), false);
        ev.headers
            .insert("Authorization".into(), "Bearer HEADERSECRET".into());
        for dbg in [format!("{sub:?}"), format!("{ev:?}")] {
            for secret in ["PATHSECRET", "QSECRET", "pw@", "HEADERSECRET"] {
                assert!(!dbg.contains(secret), "{secret} in {dbg}");
            }
            assert!(dbg.contains("https://hooks.example.com"), "{dbg}");
        }
    }

    /// #1852 — a transport error does not quote the URL; its path can be a secret.
    #[tokio::test]
    async fn a_transport_error_does_not_quote_the_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/services/T0/B0/PATHSECRET");
        let err = deliver(&event(url, true)).await.unwrap_err();
        let msg = format!("{err:?}");
        assert!(matches!(err, JobError::Retryable(_)), "{msg}");
        assert!(!msg.contains("PATHSECRET"), "{msg}");
    }

    fn event(url: String, allow_private_targets: bool) -> WebhookEvent {
        WebhookEvent {
            id: "id".into(),
            event: "ping".into(),
            target_url: url,
            body: "{}".into(),
            signature: "sha256=00".into(),
            headers: HashMap::new(),
            timeout_secs: 5,
            retry_status_codes: Vec::new(),
            allow_private_targets,
        }
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let h = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, h)
    }

    /// Deliveries share one connection; the default User-Agent yields to
    /// a subscription header (#1792).
    #[tokio::test]
    async fn deliveries_reuse_a_client_and_default_the_user_agent() {
        use axum::extract::ConnectInfo;
        use std::net::SocketAddr;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let app = Router::new().route(
            "/hook",
            post(
                move |ConnectInfo(a): ConnectInfo<SocketAddr>, h: axum::http::HeaderMap| {
                    let ua: Vec<String> = h
                        .get_all("user-agent")
                        .iter()
                        .map(|v| v.to_str().unwrap().to_owned())
                        .collect();
                    s.lock().unwrap().push((a, ua));
                    async { "ok" }
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let svc = app.into_make_service_with_connect_info::<SocketAddr>();
        let srv = tokio::spawn(async move { axum::serve(listener, svc).await.unwrap() });
        deliver(&event(url.clone(), true)).await.unwrap();
        let mut custom = event(url, true);
        custom
            .headers
            .insert("User-Agent".into(), "custom/1".into());
        deliver(&custom).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1, [USER_AGENT]);
        assert_eq!(seen[1].1, ["custom/1"]);
        assert_eq!(seen[0].0, seen[1].0, "one pooled connection");
        srv.abort();
    }

    #[tokio::test]
    async fn refuses_private_targets_by_default() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { "ok" }
            }),
        );
        let (base, srv) = serve(app).await;
        let port = base.rsplit(':').next().unwrap();
        for url in [
            format!("{base}/hook"),
            format!("http://localhost:{port}/hook"),
            format!("http://[::ffff:127.0.0.1]:{port}/hook"),
            "ftp://example.com/hook".to_owned(),
        ] {
            let err = deliver(&event(url.clone(), false)).await.unwrap_err();
            assert!(matches!(err, JobError::Fatal(_)), "{url}: {err:?}");
            let msg = format!("{err:?}");
            assert!(!msg.contains("127.0.0.1") && !msg.contains("::1"), "{msg}");
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no request reached the server"
        );
        srv.abort();
    }

    #[tokio::test]
    async fn operator_allowlist_does_not_open_a_subscription() {
        let _g = crate::outbound::ENV_LOCK.lock().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { "ok" }
            }),
        );
        let (base, srv) = serve(app).await;
        std::env::set_var(crate::outbound::ALLOW_ENV, "127.0.0.0/8,localhost");
        let err = deliver(&event(format!("{base}/hook"), false)).await;
        std::env::remove_var(crate::outbound::ALLOW_ENV);
        assert!(matches!(err, Err(JobError::Fatal(_))), "{err:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "request reached the server");
        srv.abort();
    }

    #[tokio::test]
    async fn subscription_refuses_a_private_target_by_default() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { "ok" }
            }),
        );
        let (base, srv) = serve(app).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        let dead = Arc::new(AtomicUsize::new(0));
        let d = dead.clone();
        q.on_dead_letter(move |_| {
            let d = d.clone();
            async move {
                d.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        q.start().await;
        WebhookSubscription::new(format!("{base}/hook"), "s")
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();
        for _ in 0..50 {
            if dead.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(dead.load(Ordering::SeqCst), 1, "dead-lettered at once");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no request was sent");
        srv.abort();
        q.shutdown().await;
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new()
            .route(
                "/hook",
                post(|| async {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, "/internal")],
                    )
                }),
            )
            .route(
                "/internal",
                post(move || {
                    h.fetch_add(1, Ordering::SeqCst);
                    async { "secret" }
                }),
            );
        let (base, srv) = serve(app).await;
        let err = deliver(&event(format!("{base}/hook"), true))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, JobError::Fatal(m) if m == "status 307"),
            "{err:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0, "redirect was followed");
        srv.abort();
    }

    #[tokio::test]
    async fn response_body_is_not_stored() {
        let app = Router::new().route(
            "/hook",
            post(|| async { (axum::http::StatusCode::BAD_REQUEST, "INTERNAL-SECRET") }),
        );
        let (base, srv) = serve(app).await;
        let err = deliver(&event(format!("{base}/hook"), true))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(!msg.contains("INTERNAL-SECRET"), "{msg}");
        assert!(
            matches!(&err, JobError::Fatal(m) if m == "status 400"),
            "{err:?}"
        );
        srv.abort();
    }
}
