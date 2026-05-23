//! Slack webhook provider for [`super::BroadcastFn`] (Django parity #418).
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
//! HTTP 2xx → `Ok(())`. Anything else returns the response body as the
//! error string so logging picks it up.
//!
//! Requires the `http-client` feature.

#![cfg(feature = "http-client")]

use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::BroadcastFn;

/// Build a [`BroadcastFn`] that posts to a Slack incoming webhook URL.
/// Uses a freshly-built `reqwest::Client` per callback construction;
/// call [`webhook_callback_with_client`] to share a client.
#[must_use]
pub fn webhook_callback(url: impl Into<String>) -> BroadcastFn {
    let client = reqwest::Client::new();
    webhook_callback_with_client(url, client)
}

/// Like [`webhook_callback`] but reuses an existing `reqwest::Client`.
/// Useful when the app already owns a configured HTTP client (timeout,
/// proxy, user-agent).
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
            Box::pin(async move {
                let payload = build_payload(value);
                let resp = client
                    .post(url.as_ref())
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| format!("slack POST failed: {e}"))?;
                if resp.status().is_success() {
                    Ok(())
                } else {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    Err(format!("slack returned {status}: {body}"))
                }
            });
        fut
    })
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

    /// Construct the callback; we don't fire HTTP here (no live test
    /// server in unit context), but verify the BroadcastFn shape is
    /// callable.
    #[tokio::test]
    async fn callback_is_callable() {
        let cb = webhook_callback("http://127.0.0.1:1");
        // Calling actually fires HTTP — to keep this test offline we
        // just check we got a closure back; runtime invocation lives
        // in the live test against wiremock.
        // The Arc<dyn Fn ...> doesn't implement common traits, so we
        // can only confirm by dropping.
        drop(cb);
    }
}
