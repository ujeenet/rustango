//! Slack webhook provider for [`super::BroadcastFn`].
//!
//! Apps wire Slack as the broadcast channel by passing the helper into
//! [`super::NotificationContext::with_broadcast`]:
//!
//! ```ignore
//! use rustango::notifications::{NotificationContext, slack};
//!
//! let ctx = NotificationContext::new()
//!     .with_broadcast(slack::webhook_callback("https://hooks.slack.com/services/T0/B0/xyz"));
//! ```
//!
//! The callback POSTs `{"text": "..."}` to the configured webhook URL.
//! Payload shape:
//!
//! - If `serde_json::Value` is a plain string, it's sent as `{"text": value}`.
//! - Otherwise the value is sent through unchanged (lets apps build
//!   richer Slack `blocks` / attachments by passing a full JSON
//!   object).
//!
//! HTTP 2xx → `Ok(())`. Anything else returns the status and at most
//! [`ERROR_BODY_MAX`] bytes of the body as the error string.
//!
//! [`webhook_callback`] refuses private and metadata addresses and never
//! follows redirects (#1716). List private hosts or CIDRs it may reach in
//! `RUSTANGO_OUTBOUND_ALLOW`.
//!
//! Requires the `http-client` feature.

#![cfg(feature = "http-client")]

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::BroadcastFn;
use crate::outbound::{bounded_text, CheckedTarget, TargetPolicy};

/// Most bytes of a failed response body kept in the error.
pub const ERROR_BODY_MAX: usize = 256;

/// Build a [`BroadcastFn`] that posts to a Slack incoming webhook URL.
/// The target is checked on every call, so the URL may come from config.
#[must_use]
pub fn webhook_callback(url: impl Into<String>) -> BroadcastFn {
    checked_callback(url.into(), TargetPolicy::from_env)
}

fn checked_callback(url: String, policy: fn() -> TargetPolicy) -> BroadcastFn {
    let url: Arc<str> = Arc::from(url);
    Arc::new(move |value: Value| {
        let url = Arc::clone(&url);
        let fut: Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> =
            Box::pin(async move {
                let target = CheckedTarget::check(&url, &policy())
                    .await
                    .map_err(|e| format!("slack target refused: {e}"))?;
                let client = target
                    .client(reqwest::Client::builder().timeout(Duration::from_secs(10)))
                    .map_err(|e| format!("slack client: {e}"))?;
                post(&client, target.url().as_str(), value).await
            });
        fut
    })
}

/// Like [`webhook_callback`] but reuses your `reqwest::Client`. Its
/// redirect and proxy settings apply and the target is **not** checked,
/// so pass only URLs you control.
#[must_use]
pub fn webhook_callback_with_client(
    url: impl Into<String>,
    client: reqwest::Client,
) -> BroadcastFn {
    let url: Arc<str> = Arc::from(url.into());
    Arc::new(move |value: Value| {
        let url = Arc::clone(&url);
        let client = client.clone();
        let fut: Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> =
            Box::pin(async move { post(&client, &url, value).await });
        fut
    })
}

async fn post(client: &reqwest::Client, url: &str, value: Value) -> Result<(), String> {
    let resp = client
        .post(url)
        .json(&build_payload(value))
        .send()
        .await
        .map_err(|e| format!("slack POST failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = bounded_text(resp, ERROR_BODY_MAX).await;
    Err(format!("slack returned {status}: {body}"))
}

/// Wrap a bare string as Slack's `{"text": "..."}` envelope; pass
/// through anything else (`{"blocks": [...]}` etc.) unchanged.
fn build_payload(value: Value) -> Value {
    match value {
        Value::String(s) => serde_json::json!({ "text": s }),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_value_wraps_into_text_envelope() {
        let v = build_payload(json!("hello world"));
        assert_eq!(v, json!({ "text": "hello world" }));
    }

    #[test]
    fn object_value_passes_through_unchanged() {
        let blocks = json!({
            "blocks": [
                { "type": "section", "text": { "type": "mrkdwn", "text": "*hi*" } }
            ]
        });
        assert_eq!(build_payload(blocks.clone()), blocks);
    }

    #[test]
    fn array_value_passes_through_unchanged() {
        let v = json!(["a", "b"]);
        assert_eq!(build_payload(v.clone()), v);
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[tokio::test]
    async fn private_target_is_refused() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move || {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { "ok" }
            }),
        );
        let base = serve(app).await;
        let cb = checked_callback(format!("{base}/hook"), TargetPolicy::public_only);
        let err = cb(json!("hi")).await.unwrap_err();
        assert!(err.contains("blocked address"), "{err}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn redirect_to_metadata_is_not_followed() {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(
                        axum::http::header::LOCATION,
                        "http://169.254.169.254/latest/meta-data/",
                    )],
                )
            }),
        );
        let base = serve(app).await;
        let cb = checked_callback(format!("{base}/hook"), || TargetPolicy::AllowPrivate);
        let err = cb(json!("hi")).await.unwrap_err();
        assert!(err.starts_with("slack returned 302"), "{err}");
    }

    #[tokio::test]
    async fn error_body_is_bounded() {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(|| async {
                (axum::http::StatusCode::BAD_REQUEST, "x".repeat(1_000_000))
            }),
        );
        let base = serve(app).await;
        let cb = checked_callback(format!("{base}/hook"), || TargetPolicy::AllowPrivate);
        let err = cb(json!("hi")).await.unwrap_err();
        assert!(err.starts_with("slack returned 400"), "{err}");
        assert!(err.len() < ERROR_BODY_MAX + 64, "{} bytes", err.len());
    }
}
