//! Progress + cancellation for long-running `tools/call` (epic #1013,
//! follow-up #1090).
//!
//! * **Progress** — when a `tools/call` carries `params._meta.progressToken`,
//!   the handler gets a live [`ProgressReporter`] on its [`McpContext`];
//!   each `report(..)` emits a `notifications/progress` over the SSE bus.
//! * **Cancellation** — an inbound `notifications/cancelled { requestId }`
//!   trips a process-global [`CancelToken`] keyed by the in-flight call's
//!   JSON-RPC id; the handler observes it cooperatively via
//!   `ctx.cancel.is_cancelled()` and bails out.
//!
//! Both ride the same in-process model as #1087 (the bus + registry are
//! process-local); cross-process cancellation is out of scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

/// Emits `notifications/progress` for a call's `progressToken`. A reporter
/// with no token (the default) is a silent no-op.
#[derive(Clone, Default)]
pub struct ProgressReporter {
    token: Option<Value>,
}

impl ProgressReporter {
    /// A no-op reporter (the call carried no `progressToken`).
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    pub(crate) fn with_token(token: Option<Value>) -> Self {
        Self { token }
    }

    /// `true` if the caller requested progress (a token is present).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.token.is_some()
    }

    /// Emit one progress update. No-op when there's no token.
    pub fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.token else { return };
        let mut params = json!({ "progressToken": token, "progress": progress });
        if let Some(t) = total {
            params["total"] = json!(t);
        }
        if let Some(m) = message {
            params["message"] = json!(m);
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        })
        .to_string();
        super::notifications::bus().send(frame);
    }
}

/// A cooperative cancellation flag observed by a running tool handler.
#[derive(Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::never()
    }
}

impl CancelToken {
    /// A token that is never cancelled (the default for calls that aren't
    /// registered for cancellation).
    #[must_use]
    pub fn never() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A token that is already cancelled — useful for tests and for callers
    /// that want to pre-empt a call before dispatch.
    #[must_use]
    pub fn cancelled() -> Self {
        let t = Self::never();
        t.trip();
        t
    }

    /// `true` once the call has been cancelled. Handlers should poll this
    /// at await points and return early.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn trip(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

fn registry() -> &'static Mutex<HashMap<String, CancelToken>> {
    static R: OnceLock<Mutex<HashMap<String, CancelToken>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a fresh [`CancelToken`] for an in-flight request id.
pub(crate) fn register(request_id: &str) -> CancelToken {
    let token = CancelToken::never();
    registry()
        .lock()
        .expect("cancel registry")
        .insert(request_id.to_owned(), token.clone());
    token
}

/// Remove a request id from the registry (call completed).
pub(crate) fn deregister(request_id: &str) {
    registry()
        .lock()
        .expect("cancel registry")
        .remove(request_id);
}

/// Cancel an in-flight request by id — invoked from a
/// `notifications/cancelled`. No-op if the id isn't (or is no longer)
/// in-flight.
pub fn cancel(request_id: &str) {
    if let Some(token) = registry().lock().expect("cancel registry").get(request_id) {
        token.trip();
    }
}

/// Extract a `progressToken` from a `tools/call` params `_meta`, if present.
pub(crate) fn progress_token(params: &Value) -> Option<Value> {
    params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn progress_emits_when_token_present_and_silent_otherwise() {
        let mut rx = super::super::notifications::bus().subscribe();
        ProgressReporter::disabled().report(0.5, Some(1.0), Some("half"));
        ProgressReporter::with_token(Some(json!("p1-unit"))).report(0.5, Some(1.0), Some("half"));
        // The notification bus is process-global; parallel tests share it, so
        // scan for *our* frame rather than assuming it's first.
        for _ in 0..200 {
            let Ok(frame) = rx.recv().await else { continue };
            let v: Value = serde_json::from_str(&frame).unwrap();
            if v["method"] == "notifications/progress" && v["params"]["progressToken"] == "p1-unit"
            {
                assert_eq!(v["params"]["progress"], 0.5);
                return;
            }
        }
        panic!("did not observe the progress notification");
    }

    #[test]
    fn cancel_trips_the_registered_token() {
        let token = register("req-42");
        assert!(!token.is_cancelled());
        cancel("req-42");
        assert!(token.is_cancelled());
        deregister("req-42");
        // After deregister, cancelling again is a harmless no-op.
        cancel("req-42");
    }
}
