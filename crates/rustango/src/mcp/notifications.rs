//! Server→client `list_changed` notifications (epic #1013, follow-up #1087).
//!
//! A process-global [`EventBus`] carries pre-serialized JSON-RPC
//! notification frames; the `GET {prefix}` SSE stream relays them to every
//! connected client. Call [`notify_tools_list_changed`] /
//! [`notify_prompts_list_changed`] / [`notify_resources_list_changed`]
//! after an **in-process** change (e.g. an admin grant edit) so clients
//! re-`*/list`.
//!
//! ## Caveat (documented limitation)
//! The bus is in-memory and process-local, so a change made by a *separate*
//! process — e.g. `manage grant-skill` — does **not** reach a running
//! server's connected clients. Across processes (or on stateless Streamable
//! HTTP with no open GET stream) the contract is "the client re-lists on its
//! next `initialize` / token issue." A shared (Redis) bus would lift this;
//! it's out of scope for this follow-up.

use std::sync::OnceLock;

use serde_json::json;

use crate::sse::EventBus;

/// The process-global MCP notification bus. Frames are JSON-RPC
/// notification messages (no `id`), serialized to strings.
#[must_use]
pub fn bus() -> &'static EventBus<String> {
    static BUS: OnceLock<EventBus<String>> = OnceLock::new();
    BUS.get_or_init(|| EventBus::new(256))
}

fn notify(method: &str) {
    let frame = json!({ "jsonrpc": "2.0", "method": method }).to_string();
    bus().send(frame);
}

/// Tell connected clients the tool list changed (`notifications/tools/list_changed`).
pub fn notify_tools_list_changed() {
    notify("notifications/tools/list_changed");
}

/// Tell connected clients the prompt list changed.
pub fn notify_prompts_list_changed() {
    notify("notifications/prompts/list_changed");
}

/// Tell connected clients the resource list changed.
pub fn notify_resources_list_changed() {
    notify("notifications/resources/list_changed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_a_jsonrpc_list_changed_frame() {
        let mut rx = bus().subscribe();
        notify_prompts_list_changed();
        let frame = rx.recv().await.expect("frame");
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/prompts/list_changed");
        assert!(v.get("id").is_none()); // notification, not a request
    }
}
