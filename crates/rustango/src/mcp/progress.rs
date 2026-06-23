//! Progress + cancellation for long-running `tools/call` (epic #1013,
//! follow-up #1090).
//!
//! * **Progress** — when a `tools/call` carries `params._meta.progressToken`,
//!   the handler gets a live [`ProgressReporter`] on its [`McpContext`];
//!   each `report(..)` emits a `notifications/progress` over the SSE bus.
//! * **Cancellation** — an inbound `notifications/cancelled { requestId }`
//!   trips a process-global [`CancelToken`] keyed by
//!   `(tenant, agent_id, request_id)` (so one agent can't cancel another's
//!   call, #1095); the handler observes it cooperatively via
//!   `ctx.cancel.is_cancelled()` and bails out. Registration is RAII
//!   ([`CancelGuard`]) so a panicking handler never leaks an entry.
//!
//! Both ride the same in-process model as #1087 (the bus + registry are
//! process-local); cross-process cancellation is out of scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

/// Emits `notifications/progress` for a call's `progressToken`, scoped to the
/// calling agent so frames never leak to other agents/tenants (#1092). A
/// reporter with no token (the default) is a silent no-op.
#[derive(Clone, Default)]
pub struct ProgressReporter {
    token: Option<Value>,
    tenant: String,
    agent_id: i64,
}

impl ProgressReporter {
    /// A no-op reporter (the call carried no `progressToken`).
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A reporter bound to the calling agent. `token` is the call's
    /// `_meta.progressToken` (`None` ⇒ no-op).
    pub(crate) fn for_agent(token: Option<Value>, tenant: String, agent_id: i64) -> Self {
        Self {
            token,
            tenant,
            agent_id,
        }
    }

    /// `true` if the caller requested progress (a token is present).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.token.is_some()
    }

    /// Emit one progress update. No-op when there's no token. The frame is
    /// scoped to the calling agent (only that agent's SSE stream sees it).
    pub fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.token else { return };
        let mut params = json!({ "progressToken": token, "progress": progress });
        if let Some(t) = total {
            params["total"] = json!(t);
        }
        if let Some(m) = message {
            params["message"] = json!(m);
        }
        let body = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        })
        .to_string();
        super::notifications::bus().send(super::notifications::ScopedFrame {
            tenant: self.tenant.clone(),
            agent_id: Some(self.agent_id),
            body,
        });
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

/// Registry key — a request id is only unique *within* an agent, and numeric
/// JSON-RPC ids collide trivially across agents. Keying by
/// `(tenant, agent_id, request_id)` stops agent A cancelling agent B's call
/// (#1095).
type CancelKey = (String, i64, String);

fn registry() -> &'static Mutex<HashMap<CancelKey, CancelToken>> {
    static R: OnceLock<Mutex<HashMap<CancelKey, CancelToken>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the registry, recovering from a poisoned mutex rather than panicking
/// (a tool handler that panicked mid-lock must not wedge cancellation for the
/// whole process). #1095.
fn registry_lock() -> std::sync::MutexGuard<'static, HashMap<CancelKey, CancelToken>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII registration of an in-flight call's [`CancelToken`], scoped to the
/// calling agent. The entry is removed on `Drop` — so a handler that returns
/// early, errors, or **panics** never leaks a registry entry (#1095). Replaces
/// the old manual `register`/`deregister` pair.
pub(crate) struct CancelGuard {
    key: CancelKey,
    token: CancelToken,
}

impl CancelGuard {
    /// Register a fresh token for `(tenant, agent_id, request_id)`.
    pub(crate) fn register(tenant: &str, agent_id: i64, request_id: &str) -> Self {
        let key = (tenant.to_owned(), agent_id, request_id.to_owned());
        let token = CancelToken::never();
        registry_lock().insert(key.clone(), token.clone());
        Self { key, token }
    }

    /// The cooperative cancel token to hand the running handler.
    pub(crate) fn token(&self) -> CancelToken {
        self.token.clone()
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        registry_lock().remove(&self.key);
    }
}

/// Cancel an in-flight request — invoked from a `notifications/cancelled`,
/// scoped to the requesting agent so it can only cancel **its own** calls.
/// No-op if no matching call is (still) in-flight.
pub fn cancel(tenant: &str, agent_id: i64, request_id: &str) {
    let key = (tenant.to_owned(), agent_id, request_id.to_owned());
    if let Some(token) = registry_lock().get(&key) {
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
        ProgressReporter::for_agent(Some(json!("p1-unit")), "acme".into(), 5).report(
            0.5,
            Some(1.0),
            Some("half"),
        );
        // The notification bus is process-global; parallel tests share it, so
        // scan for *our* frame rather than assuming it's first.
        for _ in 0..200 {
            let Ok(frame) = rx.recv().await else { continue };
            let v: Value = serde_json::from_str(&frame.body).unwrap();
            if v["method"] == "notifications/progress" && v["params"]["progressToken"] == "p1-unit"
            {
                assert_eq!(frame.tenant, "acme");
                assert_eq!(frame.agent_id, Some(5));
                assert_eq!(v["params"]["progress"], 0.5);
                return;
            }
        }
        panic!("did not observe the progress notification");
    }

    #[test]
    fn cancel_is_agent_scoped_and_guard_deregisters_on_drop() {
        let guard = CancelGuard::register("acme", 1, "req-42");
        let token = guard.token();
        assert!(!token.is_cancelled());

        // Same request id, different agent / tenant → must NOT trip it (#1095).
        cancel("acme", 2, "req-42");
        cancel("evil", 1, "req-42");
        assert!(!token.is_cancelled());

        // The matching (tenant, agent_id, request_id) cancels.
        cancel("acme", 1, "req-42");
        assert!(token.is_cancelled());

        // Dropping the guard removes the entry; cancelling again is a no-op.
        drop(guard);
        cancel("acme", 1, "req-42");
    }
}
