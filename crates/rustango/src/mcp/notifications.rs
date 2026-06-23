//! Server→client notifications (epic #1013, #1087) — **scoped** so they
//! never cross tenant/agent boundaries (review fix, #1092/#1093).
//!
//! A process-global [`EventBus`] carries [`ScopedFrame`]s — a JSON-RPC
//! notification body tagged with the `tenant` it belongs to and, for
//! per-agent events, the `agent_id`. The authenticated `GET {prefix}` SSE
//! stream subscribes once and relays a frame only when
//! [`frame_visible`] passes for the connected agent. `list_changed` for a
//! single agent's grants carries `agent_id: Some`; tenant-wide catalog
//! changes carry `None` (visible to every agent in that tenant).
//!
//! ## Caveat (documented limitation)
//! The bus is in-memory and process-local: a change made by a *separate*
//! process — e.g. `manage grant-skill` — does not reach a running server's
//! clients. In-process grants (apps calling `grant_skill_pool` in a request
//! handler) do. Cross-process the contract is "re-list on next
//! `initialize`"; a shared (Redis) bus would lift it.

use std::sync::OnceLock;

use serde_json::json;

use crate::sse::EventBus;

/// A notification frame tagged with its tenant/agent scope.
#[derive(Clone, Debug)]
pub struct ScopedFrame {
    /// Tenant slug the frame belongs to.
    pub tenant: String,
    /// `Some(agent_id)` for a per-agent event; `None` for tenant-wide.
    pub agent_id: Option<i64>,
    /// The serialized JSON-RPC notification message.
    pub body: String,
}

/// The process-global MCP notification bus.
#[must_use]
pub fn bus() -> &'static EventBus<ScopedFrame> {
    static BUS: OnceLock<EventBus<ScopedFrame>> = OnceLock::new();
    BUS.get_or_init(|| EventBus::new(256))
}

/// Whether a frame should be delivered to a subscriber authenticated as
/// `(tenant, agent_id)`: same tenant, and either tenant-wide or this agent.
#[must_use]
pub fn frame_visible(frame: &ScopedFrame, tenant: &str, agent_id: i64) -> bool {
    frame.tenant == tenant && frame.agent_id.map_or(true, |a| a == agent_id)
}

fn notify(tenant: &str, agent_id: Option<i64>, method: &str) {
    let body = json!({ "jsonrpc": "2.0", "method": method }).to_string();
    bus().send(ScopedFrame {
        tenant: tenant.to_owned(),
        agent_id,
        body,
    });
}

/// Notify that an agent's (or, with `agent_id: None`, a tenant's) tool list
/// changed (`notifications/tools/list_changed`).
pub fn notify_tools_list_changed(tenant: &str, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/tools/list_changed");
}

/// Notify that the prompt list changed.
pub fn notify_prompts_list_changed(tenant: &str, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/prompts/list_changed");
}

/// Notify that the resource list changed.
pub fn notify_resources_list_changed(tenant: &str, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/resources/list_changed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_is_tenant_and_agent_scoped() {
        let per_agent = ScopedFrame {
            tenant: "acme".into(),
            agent_id: Some(1),
            body: String::new(),
        };
        assert!(frame_visible(&per_agent, "acme", 1));
        assert!(!frame_visible(&per_agent, "acme", 2)); // other agent, same tenant
        assert!(!frame_visible(&per_agent, "other", 1)); // other tenant

        let tenant_wide = ScopedFrame {
            tenant: "acme".into(),
            agent_id: None,
            body: String::new(),
        };
        assert!(frame_visible(&tenant_wide, "acme", 1));
        assert!(frame_visible(&tenant_wide, "acme", 999)); // any agent in tenant
        assert!(!frame_visible(&tenant_wide, "other", 1)); // not other tenants
    }

    #[tokio::test]
    async fn notify_emits_a_scoped_jsonrpc_frame() {
        let mut rx = bus().subscribe();
        notify_prompts_list_changed("acme", Some(7));
        for _ in 0..200 {
            let Ok(frame) = rx.recv().await else { continue };
            if frame.tenant == "acme" && frame.agent_id == Some(7) {
                let v: serde_json::Value = serde_json::from_str(&frame.body).unwrap();
                assert_eq!(v["method"], "notifications/prompts/list_changed");
                assert!(v.get("id").is_none());
                return;
            }
        }
        panic!("did not observe the scoped frame");
    }
}
