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
//! - `X-Webhook-Timestamp: <unix seconds at dispatch>`
//! - `X-Webhook-Signature-V2: <signature>` over the id, event, timestamp
//!   and body; check it with [`verify_delivery`], which also bounds the age.
//!   The timestamp is set at dispatch and retries reuse it: the default
//!   retries end within about four minutes, `Retry-After` included, so
//!   `max_age` must cover that, any longer retry window you set, and any
//!   queue backlog.
//! - `X-Webhook-Signature: <signature>` over the body only, for receivers
//!   not yet on v2. It does not stop a replay under a new id or event:
//!   move receivers to v2. Both are in your chosen [`SignatureFormat`].
//! - Any extra headers from [`WebhookSubscription::header`]. One replaces
//!   a default such as `Content-Type`; it never replaces an `X-Webhook-*` header,
//!   and `Host`, `Content-Length` and `Transfer-Encoding` are refused.
//!
//! ## Retry policy
//!
//! - 2xx: done.
//! - 408, 429 and 5xx: retried with backoff, 8 attempts in all, 1s
//!   doubling. Change both with [`WebhookSubscription::max_attempts`] and
//!   [`WebhookSubscription::retry_backoff`].
//! - A `Retry-After` on 429 or 503, in seconds or as an HTTP date, makes
//!   the next retry wait at least that long, but never past two minutes
//!   after dispatch, so the signed timestamp stays fresh.
//! - Other 4xx: dead-lettered at once. A bad URL or bad auth will not
//!   fix itself.
//! - Transport errors (refused connection, DNS, TLS): retried.
//! - A request that will not build (a bad header): dead-lettered.
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
//! [`verify_delivery`]: crate::webhook::verify_delivery
//! [`WebhookSubscription::header`]: crate::webhook_delivery::WebhookSubscription::header
//! [`WebhookSubscription::retry_status_codes`]: crate::webhook_delivery::WebhookSubscription::retry_status_codes
//! [`WebhookSubscription::max_attempts`]: crate::webhook_delivery::WebhookSubscription::max_attempts
//! [`WebhookSubscription::retry_backoff`]: crate::webhook_delivery::WebhookSubscription::retry_backoff
//! [`WebhookSubscription::allow_private_targets`]: crate::webhook_delivery::WebhookSubscription::allow_private_targets

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::jobs::{exponential_backoff, Job, JobError, JobQueue};
use crate::outbound::{self, TargetError, TargetPolicy};
use crate::webhook::{sign as sign_body, sign_delivery, SignatureFormat};

/// Per-delivery UUID. Receivers can use it to drop duplicates.
pub const HEADER_ID: &str = "X-Webhook-Id";
/// Header that carries the event name.
pub const HEADER_EVENT: &str = "X-Webhook-Event";
/// Header that carries the HMAC signature of the body (v1).
pub const HEADER_SIGNATURE: &str = "X-Webhook-Signature";
/// Header that carries the dispatch time, unix seconds.
pub const HEADER_TIMESTAMP: &str = "X-Webhook-Timestamp";
/// Header that carries the v2 signature, see [`crate::webhook::verify_delivery`].
pub const HEADER_SIGNATURE_V2: &str = "X-Webhook-Signature-V2";

/// User-Agent advertised on every delivery.
pub static USER_AGENT: &str = concat!("rustango-webhook/", env!("CARGO_PKG_VERSION"));

/// One outbound webhook event: the [`Job`] payload the queue stores,
/// retries and finally delivers or dead-letters.
///
/// [`WebhookSubscription::dispatch`] builds these for you.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "QueuedEvent")]
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

/// A queued row in either shape: rows queued before the body was signed at
/// dispatch carry the secret and the raw payload, and still have to deliver.
#[derive(Deserialize)]
#[serde(untagged)]
enum QueuedEvent {
    Signed(SignedEvent),
    Legacy(LegacyEvent),
}

/// The current shape, as [`WebhookEvent`] serializes it.
#[derive(Deserialize)]
struct SignedEvent {
    id: String,
    event: String,
    target_url: String,
    body: String,
    signature: String,
    headers: HashMap<String, String>,
    timeout_secs: u64,
    retry_status_codes: Vec<u16>,
    #[serde(default)]
    allow_private_targets: bool,
}

#[derive(Deserialize)]
struct LegacyEvent {
    id: String,
    event: String,
    target_url: String,
    signing_secret: String,
    signature_format: SignatureFormat,
    payload: serde_json::Value,
    headers: HashMap<String, String>,
    timeout_secs: u64,
    retry_status_codes: Vec<u16>,
    #[serde(default)]
    allow_private_targets: bool,
}

impl TryFrom<QueuedEvent> for WebhookEvent {
    type Error = String;

    fn try_from(q: QueuedEvent) -> Result<Self, String> {
        let e = match q {
            QueuedEvent::Signed(e) => e,
            QueuedEvent::Legacy(e) => {
                // Signed here as the older release did at delivery.
                let body = serde_json::to_string(&e.payload).map_err(|e| e.to_string())?;
                let signature = sign_body(
                    e.signature_format,
                    e.signing_secret.as_bytes(),
                    body.as_bytes(),
                )
                .map_err(|e| e.to_string())?;
                SignedEvent {
                    id: e.id,
                    event: e.event,
                    target_url: e.target_url,
                    body,
                    signature,
                    headers: e.headers,
                    timeout_secs: e.timeout_secs,
                    retry_status_codes: e.retry_status_codes,
                    allow_private_targets: e.allow_private_targets,
                }
            }
        };
        Ok(Self {
            id: e.id,
            event: e.event,
            target_url: e.target_url,
            body: e.body,
            signature: e.signature,
            headers: e.headers,
            timeout_secs: e.timeout_secs,
            retry_status_codes: e.retry_status_codes,
            allow_private_targets: e.allow_private_targets,
        })
    }
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
    /// `1s * 2^attempt` backoff that spans about two minutes. Set it per
    /// subscription with [`WebhookSubscription::max_attempts`].
    const MAX_ATTEMPTS: u32 = 8;

    async fn run(&self) -> Result<(), JobError> {
        deliver(self).await
    }
}

/// The job [`WebhookSubscription::register`] runs: the event plus the
/// subscription's retry settings, left out of the row when unset.
#[derive(Serialize, Deserialize)]
struct Delivery {
    #[serde(flatten)]
    event: WebhookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backoff_ms: Option<u64>,
}

#[async_trait::async_trait]
impl Job for Delivery {
    const NAME: &'static str = WebhookEvent::NAME;
    const MAX_ATTEMPTS: u32 = WebhookEvent::MAX_ATTEMPTS;

    fn max_attempts(&self) -> u32 {
        self.max_attempts.unwrap_or(Self::MAX_ATTEMPTS)
    }

    fn retry_delay(&self, failed_attempt: u32) -> Duration {
        self.backoff_ms.map_or_else(
            || Self::retry_backoff(failed_attempt),
            |ms| exponential_backoff(Duration::from_millis(ms), failed_attempt),
        )
    }

    async fn run(&self) -> Result<(), JobError> {
        deliver(&self.event).await
    }
}

/// A `Retry-After` never delays a retry past this long after dispatch:
/// retries reuse the dispatch timestamp, which the receiver's `max_age` bounds.
const RETRY_AFTER_HORIZON: Duration = Duration::from_secs(120);

/// A `Retry-After` in seconds or as an HTTP date, capped at the horizon
/// from `dispatched_at` (unix seconds); a past date is 0.
fn retry_after(
    headers: &reqwest::header::HeaderMap,
    now: chrono::DateTime<chrono::Utc>,
    dispatched_at: Option<i64>,
) -> Option<Duration> {
    let v = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let wait = match v.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => {
            let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
            (at.with_timezone(&chrono::Utc) - now)
                .to_std()
                .unwrap_or_default()
        }
    };
    let age = dispatched_at
        .and_then(|at| u64::try_from(now.timestamp().saturating_sub(at)).ok())
        .map_or(Duration::ZERO, Duration::from_secs);
    Some(wait.min(RETRY_AFTER_HORIZON.saturating_sub(age)))
}

/// Header names the HTTP client owns: a subscription value would break
/// the request framing or its target.
fn is_client_owned(name: &str) -> bool {
    ["host", "content-length", "transfer-encoding"]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
}

/// The headers sent: a subscription header replaces a default, and
/// never one the framework signs or owns.
fn delivery_headers(event: &WebhookEvent) -> Result<reqwest::header::HeaderMap, JobError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE, USER_AGENT as UA};
    // A bad header never builds: retrying cannot fix it.
    let bad = |what: &str| JobError::Fatal(format!("request: invalid header {what}"));
    let value = |v: &str| HeaderValue::from_str(v).map_err(|_| bad("value"));
    let mut map = HeaderMap::new();
    map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    map.insert(UA, HeaderValue::from_static(USER_AGENT));
    // Sorted: two spellings of one name in an older row pick a stable winner.
    let mut extra: Vec<_> = event.headers.iter().collect();
    extra.sort();
    for (k, v) in extra {
        if is_client_owned(k) {
            return Err(bad("name"));
        }
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| bad("name"))?;
        map.insert(name, value(v)?);
    }
    // Last, so a subscription header of the same name is replaced.
    map.insert(HEADER_ID, value(&event.id)?);
    map.insert(HEADER_EVENT, value(&event.event)?);
    map.insert(HEADER_SIGNATURE, value(&event.signature)?);
    Ok(map)
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

    let req = target
        .request(reqwest::Method::POST)
        .timeout(Duration::from_secs(event.timeout_secs.max(1)))
        .headers(delivery_headers(event)?)
        .body(event.body.clone());

    let resp = match req.send().await {
        Ok(r) => r,
        // A bad header never builds: retrying cannot fix it (#2236).
        Err(e) if e.is_builder() => {
            return Err(JobError::Fatal(format!("request: {}", e.without_url())));
        }
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
    if status == 429 || status == 503 {
        let at = event
            .headers
            .get(HEADER_TIMESTAMP)
            .and_then(|v| v.parse().ok());
        if let Some(wait) = retry_after(resp.headers(), chrono::Utc::now(), at) {
            crate::jobs::retry_after(wait);
        }
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
    max_attempts: Option<u32>,
    retry_backoff: Option<Duration>,
    /// The first header [`Self::header`] refused; `dispatch` fails on it (#2236).
    bad_header: Option<String>,
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
            .field("max_attempts", &self.max_attempts)
            .field("retry_backoff", &self.retry_backoff)
            .field("bad_header", &self.bad_header)
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
            max_attempts: None,
            retry_backoff: None,
            bad_header: None,
        }
    }

    #[must_use]
    pub fn signature_format(mut self, fmt: SignatureFormat) -> Self {
        self.signature_format = fmt;
        self
    }

    /// Add a header to every delivery. It replaces a default such as
    /// `Content-Type`; the `X-Webhook-*` names the framework sets are
    /// ignored. An invalid name or value, or `Host`, `Content-Length` or
    /// `Transfer-Encoding`, is dropped and makes [`Self::dispatch`] fail
    /// with [`JobError::Fatal`].
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let (name, value) = (name.into(), value.into());
        let bad = if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
            // Length only: a pasted "Authorization: Bearer …" name holds a secret.
            format!("invalid header name ({} bytes)", name.len())
        } else if is_client_owned(&name) {
            format!("header {name:?} is set by the HTTP client")
        } else if reqwest::header::HeaderValue::from_str(&value).is_err() {
            // The value often carries auth: name only.
            format!("invalid value for header {name:?}")
        } else {
            // Names are case-insensitive: the later call wins.
            self.headers.retain(|k, _| !k.eq_ignore_ascii_case(&name));
            self.headers.insert(name, value);
            return self;
        };
        if self.bad_header.is_none() {
            self.bad_header = Some(bad);
        }
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

    /// Runs in all before a delivery dead-letters. Default 8, which with
    /// the default backoff gives up after about two minutes. 0 counts as 1.
    #[must_use]
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = Some(attempts);
        self
    }

    /// Wait before the first retry, doubled for each later one up to
    /// 1024 times. Default 1s.
    #[must_use]
    pub fn retry_backoff(mut self, base: Duration) -> Self {
        self.retry_backoff = Some(base);
        self
    }

    /// Register the delivery [`Job`] on `queue`. Call once at startup,
    /// before [`Self::dispatch`]. Safe to call twice. Registering
    /// [`WebhookEvent`] yourself instead ignores [`Self::retry_backoff`].
    pub async fn register<Q: JobQueue>(queue: &Q) {
        queue.register::<Delivery>().await;
    }

    /// Queue a [`WebhookEvent`] and return its id at once. A worker
    /// delivers it later.
    ///
    /// # Errors
    /// [`JobError::Queue`] if the enqueue fails: database down, channel
    /// closed, or a payload that will not serialize.
    /// [`JobError::Fatal`] if [`Self::header`] got an invalid header.
    pub async fn dispatch<Q: JobQueue>(
        &self,
        queue: &Q,
        event_name: impl Into<String>,
        payload: impl Serialize,
    ) -> Result<String, JobError> {
        if let Some(bad) = &self.bad_header {
            return Err(JobError::Fatal(bad.clone()));
        }
        let id = Uuid::new_v4().to_string();
        let event_name = event_name.into();
        let body = serde_json::to_string(&payload)
            .map_err(|e| JobError::Queue(format!("payload serialize: {e}")))?;
        // In `headers`, so rows already queued keep their shape.
        let at = chrono::Utc::now().timestamp();
        let v2 = sign_delivery(
            self.signature_format,
            self.secret.as_bytes(),
            &id,
            &event_name,
            at,
            body.as_bytes(),
        )
        .map_err(|e| JobError::Queue(e.to_string()))?;
        let mut headers = self.headers.clone();
        headers.retain(|k, _| {
            !k.eq_ignore_ascii_case(HEADER_TIMESTAMP)
                && !k.eq_ignore_ascii_case(HEADER_SIGNATURE_V2)
        });
        headers.insert(HEADER_TIMESTAMP.to_owned(), at.to_string());
        headers.insert(HEADER_SIGNATURE_V2.to_owned(), v2);
        let event = WebhookEvent {
            id: id.clone(),
            event: event_name,
            target_url: self.target_url.clone(),
            signature: sign_body(
                self.signature_format,
                self.secret.as_bytes(),
                body.as_bytes(),
            )
            .map_err(|e| JobError::Queue(e.to_string()))?,
            body,
            headers,
            timeout_secs: self.timeout.as_secs().max(1),
            retry_status_codes: self.retry_status_codes.clone(),
            allow_private_targets: self.allow_private_targets,
        };
        let delivery = Delivery {
            event,
            max_attempts: self.max_attempts,
            backoff_ms: self
                .retry_backoff
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        };
        queue.dispatch(&delivery).await?;
        Ok(id)
    }
}

// How apps usually share one subscription across handlers.
pub type SharedSubscription = Arc<WebhookSubscription>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::InMemoryJobQueue;
    use axum::response::IntoResponse as _;
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
                    // A repeated header shows as its values joined by `\n`.
                    let mut hdrs: HashMap<String, String> = HashMap::new();
                    for (k, v) in parts.headers.iter() {
                        if let Ok(s) = v.to_str() {
                            hdrs.entry(k.as_str().to_owned())
                                .and_modify(|e| {
                                    e.push('\n');
                                    e.push_str(s);
                                })
                                .or_insert_with(|| s.to_owned());
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

    /// A subscription header replaces a default; the framework's own
    /// `X-Webhook-*` names are never sent twice.
    #[tokio::test]
    async fn subscription_headers_never_duplicate_defaults() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let (url, srv) = start_server(200, received.clone()).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;

        let id = WebhookSubscription::new(url, "secret")
            .allow_private_targets(true)
            .header("Content-Type", "application/cloudevents+json")
            .header("x-webhook-event", "spoofed")
            .header("X-Webhook-Id", "spoofed")
            .header("X-Webhook-Signature", "spoofed")
            .header("x-tenant", "a")
            .header("X-Tenant", "b")
            .dispatch(&q, "order.created", &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        for _ in 0..50 {
            if !received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recv = received.lock().unwrap();
        let hdrs = &recv[0].1;
        let get = |n: &str| hdrs.get(n).map(String::as_str);
        assert_eq!(get("content-type"), Some("application/cloudevents+json"));
        assert_eq!(get("x-webhook-event"), Some("order.created"));
        assert_eq!(get("x-webhook-id"), Some(id.as_str()));
        assert!(!get("x-webhook-signature").unwrap().contains("spoofed"));
        // The later call wins, whatever its case.
        assert_eq!(get("x-tenant"), Some("b"));
        drop(recv);
        srv.abort();
        q.shutdown().await;
    }

    /// The v2 signature covers the id, event and timestamp too, so a
    /// captured delivery cannot be replayed under a fresh id or another
    /// event; the body-only v1 header is still sent for old receivers.
    #[tokio::test]
    async fn delivery_signs_id_event_and_timestamp() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let (url, srv) = start_server(200, received.clone()).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;
        let before = chrono::Utc::now().timestamp();
        let id = WebhookSubscription::new(url, "secret-bytes")
            .allow_private_targets(true)
            .signature_format(SignatureFormat::HexSha256)
            .dispatch(&q, "order.created", &serde_json::json!({"order_id": 7}))
            .await
            .unwrap();
        for _ in 0..50 {
            if !received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recv = received.lock().unwrap();
        let (_status, hdrs, body) = &recv[0];
        let ts = hdrs
            .get("x-webhook-timestamp")
            .expect("no timestamp header");
        let at: i64 = ts.parse().expect("unix seconds");
        assert!(
            (before..=chrono::Utc::now().timestamp()).contains(&at),
            "{at}"
        );
        let v2 = hdrs.get("x-webhook-signature-v2").expect("no v2 signature");
        let signed = |id: &str, event: &str| {
            let mut msg = format!("v2\n{id}\n{event}\n{ts}\n").into_bytes();
            msg.extend_from_slice(body);
            sign_body(SignatureFormat::HexSha256, b"secret-bytes", &msg).unwrap()
        };
        assert_eq!(v2, &signed(&id, "order.created"));
        assert_ne!(v2, &signed("other-id", "order.created"));
        assert_ne!(v2, &signed(&id, "order.deleted"));
        let v1 = sign_body(SignatureFormat::HexSha256, b"secret-bytes", body).unwrap();
        assert_eq!(hdrs.get("x-webhook-signature"), Some(&v1));
        drop(recv);
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

    /// A 429's `Retry-After` holds the retry back past the 1s backoff.
    #[tokio::test]
    async fn a_retry_after_on_429_delays_the_retry() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                let mut seen = s.lock().unwrap();
                seen.push(std::time::Instant::now());
                let first = seen.len() == 1;
                async move {
                    if first {
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "3")],
                        )
                            .into_response()
                    } else {
                        "ok".into_response()
                    }
                }
            }),
        );
        let (base, srv) = serve(app).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;
        WebhookSubscription::new(format!("{base}/hook"), "s")
            .allow_private_targets(true)
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while seen.lock().unwrap().len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "retried once");
        let gap = seen[1] - seen[0];
        assert!(gap >= Duration::from_millis(2900), "{gap:?}");
        srv.abort();
        q.shutdown().await;
    }

    /// Past the horizon from the queued timestamp, `Retry-After` no longer delays.
    #[tokio::test]
    async fn retry_after_is_not_honoured_past_the_horizon() {
        let seen = Arc::new(AtomicUsize::new(0));
        let s = seen.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                let first = s.fetch_add(1, Ordering::SeqCst) == 0;
                async move {
                    if first {
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "600")],
                        )
                            .into_response()
                    } else {
                        "ok".into_response()
                    }
                }
            }),
        );
        let (base, srv) = serve(app).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        q.start().await;
        let mut ev = event(format!("{base}/hook"), true);
        let old = chrono::Utc::now().timestamp() - 200;
        ev.headers.insert(HEADER_TIMESTAMP.into(), old.to_string());
        q.dispatch(&Delivery {
            event: ev,
            max_attempts: None,
            backoff_ms: Some(50),
        })
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while seen.load(Ordering::SeqCst) < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "the retry waited for Retry-After"
        );
        srv.abort();
        q.shutdown().await;
    }

    /// The subscription's attempts and backoff replace the 8-run default.
    #[tokio::test]
    async fn the_subscription_sets_attempts_and_backoff() {
        let (url, srv) = start_server(503, Arc::default()).await;
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        let dead = Arc::new(Mutex::new(Vec::new()));
        let d = dead.clone();
        q.on_dead_letter(move |dl| {
            let d = d.clone();
            async move { d.lock().unwrap().push(dl) }
        })
        .await;
        q.start().await;
        WebhookSubscription::new(url, "s")
            .allow_private_targets(true)
            .max_attempts(3)
            .retry_backoff(Duration::from_millis(20))
            .dispatch(&q, "ping", &serde_json::json!({}))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while dead.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let dead = dead.lock().unwrap();
        assert_eq!(dead.len(), 1, "not dead-lettered within 3s");
        assert_eq!(dead[0].attempts, 3);
        assert_eq!(dead[0].payload["max_attempts"], 3);
        srv.abort();
        q.shutdown().await;
    }

    #[test]
    fn retry_after_reads_seconds_and_dates_capped() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-27T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let of = |v: &str| {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(reqwest::header::RETRY_AFTER, v.parse().unwrap());
            retry_after(&h, now, None)
        };
        assert_eq!(of("90"), Some(Duration::from_secs(90)));
        assert_eq!(
            of("Tue, 27 Oct 2026 12:01:00 GMT"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(of("Tue, 27 Oct 2026 11:00:00 GMT"), Some(Duration::ZERO));
        assert_eq!(of("999999"), Some(RETRY_AFTER_HORIZON));
        assert_eq!(of("soon"), None);
        assert_eq!(
            retry_after(&reqwest::header::HeaderMap::new(), now, None),
            None
        );
    }

    /// Without retry settings the row has the shape older releases read.
    #[test]
    fn a_delivery_without_retry_settings_keeps_the_event_shape() {
        let ev = event("https://example.com/h".into(), false);
        let plain = Delivery {
            event: ev.clone(),
            max_attempts: None,
            backoff_ms: None,
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::to_value(&ev).unwrap()
        );
        let set = Delivery {
            event: ev,
            max_attempts: Some(20),
            backoff_ms: Some(5_000),
        };
        let back: Delivery = serde_json::from_value(serde_json::to_value(&set).unwrap()).unwrap();
        assert_eq!(Job::max_attempts(&back), 20);
        assert_eq!(back.retry_delay(2), Duration::from_secs(20));
        let old: Delivery = serde_json::from_value(serde_json::to_value(&plain).unwrap()).unwrap();
        assert_eq!(Job::max_attempts(&old), 8);
        assert_eq!(old.retry_delay(0), Duration::from_secs(1));
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

    /// A dead-lettered row an older release queued reaches the callback
    /// in the current shape, without the signing secret it stored.
    #[tokio::test]
    async fn an_older_row_dead_letters_without_its_secret() {
        #[derive(Serialize, Deserialize)]
        #[serde(transparent)]
        struct OlderRow(serde_json::Value);
        #[async_trait::async_trait]
        impl Job for OlderRow {
            const NAME: &'static str = WebhookEvent::NAME;
            async fn run(&self) -> Result<(), JobError> {
                unreachable!("WebhookEvent's handler runs it")
            }
        }
        let (url, srv) = start_server(404, Arc::default()).await;
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
        let old = serde_json::json!({
            "id": "id", "event": "ping", "target_url": url,
            "signing_secret": "s3cret-older", "signature_format": "HexSha256WithPrefix",
            "payload": {"a": 1}, "headers": {}, "timeout_secs": 5,
            "retry_status_codes": [], "allow_private_targets": true,
        });
        q.dispatch(&OlderRow(old)).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while dead.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let payload = dead.lock().unwrap().first().expect("dead-lettered").clone();
        assert!(!payload.to_string().contains("s3cret-older"), "{payload}");
        assert_eq!(payload["body"], r#"{"a":1}"#, "{payload}");
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

    /// #2236 — a bad header is refused at dispatch, not retried as transport.
    #[tokio::test]
    async fn an_invalid_header_fails_dispatch_fatally() {
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        for sub in [
            WebhookSubscription::new("https://example.com/hook", "s")
                .header("Authorization: Bearer SECRET", "v"),
            WebhookSubscription::new("https://example.com/hook", "s")
                .header("X-Ok", "line\nbreak-SECRET"),
        ] {
            let dbg = format!("{sub:?}");
            assert!(!dbg.contains("SECRET"), "{dbg}");
            let err = sub.dispatch(&q, "ping", &()).await.unwrap_err();
            let msg = format!("{err:?}");
            assert!(matches!(err, JobError::Fatal(_)), "{msg}");
            assert!(!msg.contains("SECRET"), "{msg}");
        }
    }

    /// #2236 — an already-queued bad header dead-letters instead of retrying.
    #[tokio::test]
    async fn a_request_build_error_is_fatal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let mut ev = event(url, true);
        ev.headers.insert("X Bad".into(), "v".into());
        let err = deliver(&ev).await.unwrap_err();
        assert!(matches!(err, JobError::Fatal(_)), "{err:?}");
    }

    /// Framing and target headers belong to the client: refused at dispatch and delivery.
    #[tokio::test]
    async fn client_owned_headers_are_refused() {
        let q = InMemoryJobQueue::with_workers(1);
        WebhookSubscription::register(&q).await;
        for name in ["Host", "content-length", "Transfer-Encoding"] {
            let err = WebhookSubscription::new("https://example.com/hook", "s")
                .header(name, "1")
                .dispatch(&q, "ping", &())
                .await
                .unwrap_err();
            assert!(matches!(err, JobError::Fatal(_)), "{name}: {err:?}");
            let mut ev = event("https://example.com/hook".into(), false);
            ev.headers.insert(name.into(), "1".into());
            assert!(
                matches!(delivery_headers(&ev), Err(JobError::Fatal(_))),
                "{name} queued"
            );
        }
    }

    /// A long `Retry-After` is cut so the retry stays near the signed timestamp.
    #[test]
    fn retry_after_stops_at_the_horizon_from_dispatch() {
        let now = chrono::Utc::now();
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "600".parse().unwrap());
        let t = now.timestamp();
        assert_eq!(retry_after(&h, now, Some(t)), Some(RETRY_AFTER_HORIZON));
        assert_eq!(
            retry_after(&h, now, Some(t - 100)),
            Some(RETRY_AFTER_HORIZON - Duration::from_secs(100))
        );
        assert_eq!(retry_after(&h, now, Some(t - 900)), Some(Duration::ZERO));
        // A clock behind the dispatcher's never extends the horizon.
        assert_eq!(
            retry_after(&h, now, Some(t + 900)),
            Some(RETRY_AFTER_HORIZON)
        );
    }

    /// A row queued by an older release, which stored the secret, still delivers.
    #[test]
    fn an_older_queued_event_still_decodes_and_signs() {
        let old = serde_json::json!({
            "id": "id", "event": "ping", "target_url": "https://example.com/h",
            "signing_secret": "s3cret", "signature_format": "HexSha256WithPrefix",
            "payload": {"a": 1}, "headers": {}, "timeout_secs": 5,
            "retry_status_codes": [],
        });
        let ev: WebhookEvent = serde_json::from_value(old).expect("the older shape decodes");
        assert_eq!(ev.body, r#"{"a":1}"#);
        assert!(crate::webhook::verify_signature(
            SignatureFormat::HexSha256WithPrefix,
            b"s3cret",
            ev.body.as_bytes(),
            &ev.signature,
        ));
        let current: WebhookEvent =
            serde_json::from_value(serde_json::to_value(&ev).unwrap()).expect("round trip");
        assert_eq!(current.signature, ev.signature);
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
