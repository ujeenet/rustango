//! Progress reports and cancellation for a slow `tools/call`.
//!
//! **Progress.** When a call carries `params._meta.progressToken`,
//! the handler gets a live [`ProgressReporter`], and each `report`
//! sends a `notifications/progress` over the SSE bus.
//!
//! **Cancellation.** An inbound `notifications/cancelled
//! { requestId }` trips a [`CancelToken`] keyed by tenant, agent and
//! request id, so no agent can cancel another's call. The handler
//! has to cooperate: poll `ctx.cancel.is_cancelled()` and return
//! early. A guard removes the registry entry on drop, so a handler
//! that panics leaks nothing.
//!
//! The bus and the registry both live in one process, so
//! cancellation does not cross process boundaries.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

/// Sends `notifications/progress` for one call's `progressToken`,
/// scoped to the calling agent so no other agent sees the frames. A
/// reporter with no token, which is the default, does nothing.
#[derive(Clone, Default)]
pub struct ProgressReporter {
    token: Option<Value>,
    tenant: String,
    agent_id: i64,
}

impl ProgressReporter {
    /// A reporter that does nothing, for a call with no token.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A reporter for one agent. `token` is the call's
    /// `_meta.progressToken`; `None` gives a reporter that does
    /// nothing.
    pub(crate) fn for_agent(token: Option<Value>, tenant: String, agent_id: i64) -> Self {
        Self {
            token,
            tenant,
            agent_id,
        }
    }

    /// Whether the caller asked for progress, that is, sent a token.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.token.is_some()
    }

    /// Send one progress update, or nothing when there is no token.
    /// Only the calling agent's stream sees it.
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

/// A flag a running tool handler is expected to poll.
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
    /// A token that is never cancelled. This is the default for a
    /// call that is not registered for cancellation.
    #[must_use]
    pub fn never() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A token that is already cancelled. Useful in tests, and to
    /// stop a call before it is dispatched.
    #[must_use]
    pub fn cancelled() -> Self {
        let t = Self::never();
        t.trip();
        t
    }

    /// Whether the call was cancelled. Poll it at every await point
    /// and return early when it is true.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn trip(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

/// The registry key. A request id is only unique within one agent,
/// and numeric ids collide across agents at once, so the key takes
/// the tenant and agent too.
type CancelKey = (String, i64, String);

fn registry() -> &'static Mutex<HashMap<CancelKey, CancelToken>> {
    static R: OnceLock<Mutex<HashMap<CancelKey, CancelToken>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the registry, recovering a poisoned mutex instead of
/// panicking. One handler that panicked while holding the lock must
/// not break cancellation for the whole process.
fn registry_lock() -> std::sync::MutexGuard<'static, HashMap<CancelKey, CancelToken>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds a call's [`CancelToken`] in the registry, and removes it on
/// drop. A handler that returns early, errors, or panics therefore
/// leaks no entry.
pub(crate) struct CancelGuard {
    key: CancelKey,
    token: CancelToken,
}

impl CancelGuard {
    /// Register a new token for this tenant, agent and request.
    pub(crate) fn register(tenant: &str, agent_id: i64, request_id: &str) -> Self {
        let key = (tenant.to_owned(), agent_id, request_id.to_owned());
        let token = CancelToken::never();
        registry_lock().insert(key.clone(), token.clone());
        Self { key, token }
    }

    /// The token to hand to the running handler.
    pub(crate) fn token(&self) -> CancelToken {
        self.token.clone()
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        registry_lock().remove(&self.key);
    }
}

/// Cancel a call in flight, from a `notifications/cancelled`. An
/// agent can only cancel **its own** calls. Does nothing when no
/// matching call is still running.
pub fn cancel(tenant: &str, agent_id: i64, request_id: &str) {
    let key = (tenant.to_owned(), agent_id, request_id.to_owned());
    if let Some(token) = registry_lock().get(&key) {
        token.trip();
    }
}

/// Read the `progressToken` out of a call's `_meta`, if it has one.
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
        // Tests in parallel share the bus, so look for our own frame
        // instead of taking the first one.
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

        // The same request id under another agent or tenant must not
        // trip this token.
        cancel("acme", 2, "req-42");
        cancel("evil", 1, "req-42");
        assert!(!token.is_cancelled());

        // A full key match does cancel it.
        cancel("acme", 1, "req-42");
        assert!(token.is_cancelled());

        // Dropping the guard removes the entry, so a second cancel
        // does nothing.
        drop(guard);
        cancel("acme", 1, "req-42");
    }
}
