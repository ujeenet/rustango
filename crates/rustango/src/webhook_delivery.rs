//! Outbound webhook delivery — POSTs an HMAC-signed JSON payload to a
//! subscriber URL via the background job queue.
//!
//! Wraps the existing [`crate::webhook`] signing format and the
//! [`crate::jobs`] retry-with-backoff machinery into a one-call API:
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
//! - `X-Webhook-Id: <uuid>` — stable per delivery, retried as-is so the
//!   receiver can dedup.
//! - `X-Webhook-Event: <event_name>`
//! - `X-Webhook-Signature: <signature>` (header + format follow your
//!   chosen [`SignatureFormat`]).
//! - Any extra headers added via [`WebhookSubscription::header`].
//!
//! ## Retry policy
//!
//! Status codes are mapped to job outcomes:
//!
//! - **2xx** — success, delivery completes.
//! - **408 Request Timeout** / **429 Too Many Requests** / **5xx** —
//!   `JobError::Retryable`; retried with exponential backoff up to
//!   `MAX_ATTEMPTS` (default 8).
//! - **other 4xx** — `JobError::Fatal`; goes straight to dead-letter,
//!   no retries (a malformed URL or auth failure won't fix itself).
//! - **transport errors** (connect refused, DNS failure, body too
//!   large, etc.) — `JobError::Retryable`.
//!
//! Customize per-subscription with [`WebhookSubscription::retry_status_codes`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::jobs::{Job, JobError, JobQueue};
use crate::webhook::{sign as sign_body, SignatureFormat};

/// Header that carries the per-delivery UUID — receivers can dedup on it.
pub const HEADER_ID: &str = "X-Webhook-Id";
/// Header that carries the event name.
pub const HEADER_EVENT: &str = "X-Webhook-Event";
/// Header that carries the HMAC signature of the body.
pub const HEADER_SIGNATURE: &str = "X-Webhook-Signature";

/// User-Agent advertised on every delivery.
pub static USER_AGENT: &str = concat!("rustango-webhook/", env!("CARGO_PKG_VERSION"));

/// One outbound webhook event — the [`Job`] payload that the queue persists,
/// retries, and eventually delivers (or dead-letters).
///
/// Constructed via [`WebhookSubscription::dispatch`] — you don't usually
/// build this directly.
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
    /// Status codes (besides 5xx, 408, 429) that should retry. Empty
    /// uses the default policy.
    pub retry_status_codes: Vec<u16>,
}

#[async_trait::async_trait]
impl Job for WebhookEvent {
    const NAME: &'static str = "rustango.webhook_delivery";
    /// Webhooks typically retry for a long time — 8 attempts with the
    /// queue's `1s · 2^attempt` backoff covers ~17 minutes.
    const MAX_ATTEMPTS: u32 = 8;

    async fn run(&self) -> Result<(), JobError> {
        deliver(self).await
    }
}

async fn deliver(event: &WebhookEvent) -> Result<(), JobError> {
    let body = serde_json::to_vec(&event.payload).map_err(|e| {
        // Bad payload — won't fix itself.
        JobError::Fatal(format!("payload serialize: {e}"))
    })?;
    let signature = sign_body(event.signature_format, event.signing_secret.as_bytes(), &body);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(event.timeout_secs.max(1)))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| JobError::Queue(format!("build http client: {e}")))?;

    let mut req = client
        .post(&event.target_url)
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
            // Transport / DNS / TLS — retry.
            return Err(JobError::Retryable(format!("transport: {e}")));
        }
    };
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    if event.retry_status_codes.contains(&status) || is_default_retryable(status) {
        let body = resp.text().await.unwrap_or_default();
        Err(JobError::Retryable(format!(
            "status {status}: {}",
            truncate(&body, 200)
        )))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(JobError::Fatal(format!(
            "status {status}: {}",
            truncate(&body, 200)
        )))
    }
}

fn is_default_retryable(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

// =====================================================================
// WebhookSubscription — fluent builder + dispatch helper
// =====================================================================

/// Static config for one webhook subscriber, plus convenience methods to
/// register the delivery handler and dispatch events.
///
/// Keep one of these per subscription. Cheap to clone.
#[derive(Debug, Clone)]
pub struct WebhookSubscription {
    target_url: String,
    secret: String,
    signature_format: SignatureFormat,
    headers: HashMap<String, String>,
    timeout: Duration,
    retry_status_codes: Vec<u16>,
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

    /// Add status codes that should be retried in addition to the
    /// defaults (408, 429, 5xx).
    #[must_use]
    pub fn retry_status_codes(mut self, codes: impl IntoIterator<Item = u16>) -> Self {
        self.retry_status_codes.extend(codes);
        self
    }

    /// Register the delivery [`Job`] on `queue`. Idempotent — call once
    /// per process at startup before [`Self::dispatch`].
    pub async fn register<Q: JobQueue>(queue: &Q) {
        queue.register::<WebhookEvent>().await;
    }

    /// Build a [`WebhookEvent`] and enqueue it. Returns immediately —
    /// delivery happens asynchronously on a worker.
    ///
    /// # Errors
    /// Returns the underlying [`JobError::Queue`] when the enqueue fails
    /// (DB unavailable, channel closed, payload not serializable).
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
        };
        queue.dispatch(&event).await?;
        Ok(id)
    }
}

// `Arc<WebhookSubscription>` is the typical way apps pass a subscription
// across handlers. The newtype lets us add methods cheaply later if
// needed.
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

    /// Spin up a tiny axum server on a random port. Returns the bound
    /// URL plus a shutdown handle. The handler stashes incoming
    /// (status,headers,body) into `received` and returns the configured
    /// `respond_status`.
    async fn start_server(
        respond_status: u16,
        received: Arc<Mutex<Vec<(reqwest::StatusCode, HashMap<String, String>, Vec<u8>)>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        // Capture the chosen status code in shared state so the handler
        // can read it without it being part of the closure type.
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
                    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
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
        // Tiny delay so the server is accept()-ing before we POST.
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
        assert_eq!(hdrs.get("x-webhook-event").map(String::as_str), Some("order.created"));
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
        // 404 — Fatal, dead-letters immediately, no retry.
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
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();

        // Wait a beat for delivery + dead-letter callback.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(received.lock().unwrap().len(), 1, "tried once, then gave up");
        assert_eq!(dl_count.load(Ordering::SeqCst), 1, "dead-letter fired");

        srv.abort();
        q.shutdown().await;
    }

    #[tokio::test]
    async fn retryable_on_5xx() {
        // First two attempts fail with 503, third succeeds. With the
        // default backoff (2s after first fail, 4s after second), wait
        // ~7 seconds.
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
                        let bytes =
                            axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
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
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();

        // 503 → 2s backoff → 503 → 4s backoff → 200. ~7s total.
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

    #[test]
    fn truncate_appends_ellipsis_only_when_over_max() {
        assert_eq!(truncate("short", 100), "short");
        let long = "a".repeat(300);
        let t = truncate(&long, 50);
        assert_eq!(t.chars().count(), 51); // 50 chars + ellipsis
        assert!(t.ends_with('…'));
    }
}
