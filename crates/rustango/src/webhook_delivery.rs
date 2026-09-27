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
//! - Body: the payload re-serialized to JSON.
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
//! multicast targets are dead-lettered, and the connection is pinned to
//! the checked addresses. [`WebhookSubscription::allow_private_targets`]
//! turns the address check off. Only the status code is kept on failure.
//!
//! [`SignatureFormat`]: crate::webhook::SignatureFormat
//! [`WebhookSubscription::header`]: crate::webhook_delivery::WebhookSubscription::header
//! [`WebhookSubscription::retry_status_codes`]: crate::webhook_delivery::WebhookSubscription::retry_status_codes
//! [`WebhookSubscription::allow_private_targets`]: crate::webhook_delivery::WebhookSubscription::allow_private_targets

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::jobs::{Job, JobError, JobQueue};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub id: String,
    pub event: String,
    pub target_url: String,
    pub signing_secret: String,
    pub signature_format: SignatureFormat,
    pub payload: Value,
    pub headers: HashMap<String, String>,
    pub timeout_secs: u64,
    /// Extra status codes to retry, on top of 408, 429 and 5xx.
    pub retry_status_codes: Vec<u16>,
    /// Allow loopback, private and link-local targets. Off by default.
    #[serde(default)]
    pub allow_private_targets: bool,
}

#[async_trait::async_trait]
impl Job for WebhookEvent {
    const NAME: &'static str = "rustango.webhook_delivery";
    /// 8 attempts in total, so 7 retries. With the queue's
    /// `1s * 2^attempt` backoff that spans about two minutes. Raise it
    /// to 11 for roughly 17 minutes.
    const MAX_ATTEMPTS: u32 = 8;

    async fn run(&self) -> Result<(), JobError> {
        deliver(self).await
    }
}

async fn deliver(event: &WebhookEvent) -> Result<(), JobError> {
    let body = serde_json::to_vec(&event.payload).map_err(|e| {
        // A bad payload will not fix itself.
        JobError::Fatal(format!("payload serialize: {e}"))
    })?;
    let signature = sign_body(
        event.signature_format,
        event.signing_secret.as_bytes(),
        &body,
    );

    let url = reqwest::Url::parse(&event.target_url)
        .map_err(|e| JobError::Fatal(format!("bad target url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(JobError::Fatal(format!(
            "scheme not allowed: {}",
            url.scheme()
        )));
    }
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(event.timeout_secs.max(1)))
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none());
    if !event.allow_private_targets {
        let (host, addrs) = checked_target(&url).await?;
        // Connect only to the checked addresses; a proxy would re-resolve the host.
        builder = builder.no_proxy();
        if let Some(host) = host {
            builder = builder.resolve_to_addrs(&host, &addrs);
        }
    }
    let client = builder
        .build()
        .map_err(|e| JobError::Queue(format!("build http client: {e}")))?;

    let mut req = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(HEADER_ID, &event.id)
        .header(HEADER_EVENT, &event.event)
        .header(HEADER_SIGNATURE, signature)
        .body(body);
    for (k, v) in &event.headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Transport, DNS or TLS: worth retrying.
            return Err(JobError::Retryable(format!("transport: {e}")));
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

/// Check every resolved address of `url`. Returns the host name (None
/// for an IP literal) and the addresses to pin it to.
async fn checked_target(url: &reqwest::Url) -> Result<(Option<String>, Vec<SocketAddr>), JobError> {
    let host = url
        .host_str()
        .ok_or_else(|| JobError::Fatal("target url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    // The error never names the address: it may be an internal one.
    let blocked = || JobError::Fatal("target resolves to a blocked address".into());
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(blocked());
        }
        return Ok((None, vec![SocketAddr::new(ip, port)]));
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| JobError::Retryable(format!("dns: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(JobError::Retryable(format!("dns: no addresses for {host}")));
    }
    if addrs.iter().any(|a| is_blocked_ip(a.ip())) {
        return Err(blocked());
    }
    Ok((Some(host.to_owned()), addrs))
}

/// Loopback, private, link-local, CGNAT, multicast, unspecified and
/// other non-public ranges. IPv4 embedded in IPv6 is checked as IPv4.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(v4);
            }
            let seg = v6.segments();
            let v4 = |hi: u16, lo: u16| {
                let [a, b] = hi.to_be_bytes();
                let [c, d] = lo.to_be_bytes();
                Ipv4Addr::new(a, b, c, d)
            };
            // NAT64 64:ff9b::/96 and IPv4-translated ::ffff:0:0:0/96.
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || seg[..6] == [0, 0, 0, 0, 0xffff, 0] {
                return is_blocked_v4(v4(seg[6], seg[7]));
            }
            // 6to4 2002::/16 carries the IPv4 in bits 16..48.
            if seg[0] == 0x2002 {
                return is_blocked_v4(v4(seg[1], seg[2]));
            }
            // Teredo 2001::/32: server IPv4, then the client IPv4 XOR'd.
            if seg[0] == 0x2001 && seg[1] == 0 {
                return is_blocked_v4(v4(seg[2], seg[3])) || is_blocked_v4(v4(!seg[6], !seg[7]));
            }
            v6.is_loopback()
                || seg[..3] == [0x64, 0xff9b, 1] // local-use NAT64 64:ff9b:1::/48
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (seg[0] & 0xffc0) == 0xfec0 // site-local fec0::/10
                || seg[..6] == [0, 0, 0, 0, 0, 0] // IPv4-compatible ::/96
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0 // this network 0.0.0.0/8
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || (a == 100 && (b & 0xc0) == 64) // CGNAT 100.64.0.0/10
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24
        || (a == 198 && (b & 0xfe) == 18) // benchmarking 198.18.0.0/15
        || a >= 240 // reserved 240.0.0.0/4
}

/// Config for one webhook subscriber, plus methods to register the
/// delivery handler and send events.
///
/// Keep one per subscription. Cheap to clone.
#[derive(Debug, Clone)]
pub struct WebhookSubscription {
    target_url: String,
    secret: String,
    signature_format: SignatureFormat,
    headers: HashMap<String, String>,
    timeout: Duration,
    retry_status_codes: Vec<u16>,
    allow_private_targets: bool,
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
        let event = WebhookEvent {
            id: id.clone(),
            event: event_name.into(),
            target_url: self.target_url.clone(),
            signing_secret: self.secret.clone(),
            signature_format: self.signature_format,
            payload: serde_json::to_value(&payload)
                .map_err(|e| JobError::Queue(format!("payload to_value: {e}")))?,
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

    fn event(url: String, allow_private_targets: bool) -> WebhookEvent {
        WebhookEvent {
            id: "id".into(),
            event: "ping".into(),
            target_url: url,
            signing_secret: "s".into(),
            signature_format: SignatureFormat::HexSha256WithPrefix,
            payload: serde_json::json!({}),
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

    #[test]
    fn blocked_ip_ranges() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "::ffff:0:7f00:1",              // IPv4-translated 127.0.0.1
            "64:ff9b:1::808:808",           // local-use NAT64
            "2002:7f00:1::1",               // 6to4 of 127.0.0.1
            "2002:a9fe:a9fe::1",            // 6to4 of 169.254.169.254
            "2001:0:808:808:0:0:80ff:fffe", // Teredo, client 127.0.0.1
            "2001:0:a00:1:0:0:f7f7:f7f7",   // Teredo, server 10.0.0.1
            "fec0::1",
            "::7f00:1",
            "100.127.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "240.0.0.1",
        ] {
            assert!(is_blocked_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in [
            "93.184.216.34",
            "8.8.8.8",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "::ffff:0:808:808",
            "2002:808:808::1",
            "2001:0:808:808:0:0:f7f7:f7f7", // Teredo, client 8.8.8.8
        ] {
            assert!(
                !is_blocked_ip(ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
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
