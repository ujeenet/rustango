//! Notifications from server to client, **scoped** so they never
//! cross a tenant or agent boundary.
//!
//! One [`EventBus`] per process carries [`ScopedFrame`]s: a JSON-RPC
//! notification tagged with its `tenant` and, for a per-agent event,
//! its `agent_id`. The SSE stream subscribes once and relays a frame
//! only when [`frame_visible`] says the connected agent may see it.
//!
//! A change to one agent's grants carries `agent_id: Some`. A change
//! to the whole tenant's catalog carries `None`, so every agent in
//! that tenant gets it.
//!
//! ## Limit
//!
//! The bus lives in memory, in one process. A change made elsewhere,
//! say by `manage grant-skill`, never reaches a running server's
//! clients; a grant made inside a request handler does. Across
//! processes the client has to re-list on its next `initialize`. A
//! shared bus, on Redis, would remove this limit.

use std::sync::OnceLock;

use serde_json::json;

use crate::sse::EventBus;

/// A notification plus the scope it belongs to.
#[derive(Clone, Debug)]
pub struct ScopedFrame {
    /// The tenant slug this frame is for.
    pub tenant: String,
    /// One agent's id, or `None` for the whole tenant.
    pub agent_id: Option<i64>,
    /// The JSON-RPC notification, already serialized.
    pub body: String,
}

/// This process's notification bus.
#[must_use]
pub fn bus() -> &'static EventBus<ScopedFrame> {
    static BUS: OnceLock<EventBus<ScopedFrame>> = OnceLock::new();
    BUS.get_or_init(|| EventBus::new(256))
}

/// Whether this agent may see a frame: it must be the same tenant,
/// and the frame must be tenant-wide or for this agent.
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

/// Say the tool list changed, for one agent or, with `None`, for the
/// whole tenant.
pub fn notify_tools_list_changed(tenant: &str, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/tools/list_changed");
}

/// Say the prompt list changed.
pub fn notify_prompts_list_changed(tenant: &str, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/prompts/list_changed");
}

/// Say the resource list changed.
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
