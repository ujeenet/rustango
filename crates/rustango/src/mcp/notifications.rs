//! Notifications from server to client, **scoped** so they never
//! cross a tenant or agent boundary.
//!
//! Each tenant has its own [`EventBus`] of [`ScopedFrame`]s: a JSON-RPC
//! notification tagged with its `tenant` and, for a per-agent event,
//! its `agent_id`. One tenant's burst cannot push another tenant's frames
//! out of a shared buffer. The SSE stream subscribes to its tenant with
//! [`subscribe`] and relays a frame only when [`frame_visible`] says the
//! connected agent may see it.
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

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::json;

use crate::sse::EventBus;
use crate::tenancy::TenantSlug;

/// A notification plus the scope it belongs to.
#[derive(Clone, Debug)]
pub struct ScopedFrame {
    /// The tenant this frame is for: its [`TenantSlug::namespace`], org id
    /// included, so a tenant that reuses a purged slug never gets it.
    pub tenant: String,
    /// One agent's id, or `None` for the whole tenant.
    pub agent_id: Option<i64>,
    /// The JSON-RPC notification, already serialized.
    pub body: String,
}

/// Frames each tenant's channel buffers for a slow stream.
const TENANT_CAPACITY: usize = 256;

/// Every tenant's frames, for in-process observers. SSE streams read
/// their tenant's channel instead; reach them with [`send`].
#[must_use]
pub fn bus() -> &'static EventBus<ScopedFrame> {
    static BUS: OnceLock<EventBus<ScopedFrame>> = OnceLock::new();
    BUS.get_or_init(|| EventBus::new(TENANT_CAPACITY))
}

fn tenant_buses() -> &'static Mutex<HashMap<String, EventBus<ScopedFrame>>> {
    static BUSES: OnceLock<Mutex<HashMap<String, EventBus<ScopedFrame>>>> = OnceLock::new();
    BUSES.get_or_init(Mutex::default)
}

/// Send `frame` to the streams of `frame.tenant`, and to [`bus`].
/// Returns how many streams got it.
pub fn send(frame: ScopedFrame) -> usize {
    if bus().receiver_count() > 0 {
        bus().send(frame.clone());
    }
    let bus = tenant_buses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&frame.tenant)
        .cloned();
    // No channel means no stream is open for this tenant.
    bus.map_or(0, |b| b.send(frame))
}

/// A receiver of `tenant`'s frames only.
#[must_use]
pub fn subscribe(tenant: &TenantSlug) -> tokio::sync::broadcast::Receiver<ScopedFrame> {
    let mut buses = tenant_buses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Channels of tenants with no open stream are dropped here.
    buses.retain(|_, b| b.receiver_count() > 0);
    buses
        .entry(tenant.namespace())
        .or_insert_with(|| EventBus::new(TENANT_CAPACITY))
        .subscribe()
}

/// Whether this agent may see a frame: it must be the same tenant,
/// and the frame must be tenant-wide or for this agent.
#[must_use]
pub fn frame_visible(frame: &ScopedFrame, tenant: &TenantSlug, agent_id: i64) -> bool {
    frame.tenant == tenant.namespace() && frame.agent_id.map_or(true, |a| a == agent_id)
}

fn notify(tenant: &TenantSlug, agent_id: Option<i64>, method: &str) {
    let body = json!({ "jsonrpc": "2.0", "method": method }).to_string();
    send(ScopedFrame {
        tenant: tenant.namespace(),
        agent_id,
        body,
    });
}

/// Say the tool list changed, for one agent or, with `None`, for the
/// whole tenant.
pub fn notify_tools_list_changed(tenant: &TenantSlug, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/tools/list_changed");
}

/// Say the prompt list changed.
pub fn notify_prompts_list_changed(tenant: &TenantSlug, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/prompts/list_changed");
}

/// Say the resource list changed.
pub fn notify_resources_list_changed(tenant: &TenantSlug, agent_id: Option<i64>) {
    notify(tenant, agent_id, "notifications/resources/list_changed");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acme() -> TenantSlug {
        TenantSlug::new(1, "acme")
    }

    /// A tenant that reuses a purged tenant's slug has a new org id: its
    /// frames never reach the old tenant's streams, and the other way round.
    #[test]
    fn a_reused_slug_does_not_see_the_purged_tenants_frames() {
        let heir = TenantSlug::new(2, "acme");
        let frame = ScopedFrame {
            tenant: heir.namespace(),
            agent_id: None,
            body: String::new(),
        };
        assert!(!frame_visible(&frame, &acme(), 1));
        assert!(frame_visible(&frame, &heir, 1));
    }

    #[test]
    fn visibility_is_tenant_and_agent_scoped() {
        let per_agent = ScopedFrame {
            tenant: acme().namespace(),
            agent_id: Some(1),
            body: String::new(),
        };
        assert!(frame_visible(&per_agent, &acme(), 1));
        assert!(!frame_visible(&per_agent, &acme(), 2)); // other agent, same tenant
        assert!(!frame_visible(&per_agent, &TenantSlug::new(2, "other"), 1)); // other tenant

        let tenant_wide = ScopedFrame {
            tenant: acme().namespace(),
            agent_id: None,
            body: String::new(),
        };
        assert!(frame_visible(&tenant_wide, &acme(), 1));
        assert!(frame_visible(&tenant_wide, &acme(), 999)); // any agent in tenant
        assert!(!frame_visible(
            &tenant_wide,
            &TenantSlug::new(2, "other"),
            1
        )); // not other tenants
    }

    #[tokio::test]
    async fn notify_emits_a_scoped_jsonrpc_frame() {
        let mut rx = subscribe(&acme());
        notify_prompts_list_changed(&acme(), Some(7));
        for _ in 0..200 {
            let Ok(frame) = rx.recv().await else { continue };
            if frame.tenant == acme().namespace() && frame.agent_id == Some(7) {
                let v: serde_json::Value = serde_json::from_str(&frame.body).unwrap();
                assert_eq!(v["method"], "notifications/prompts/list_changed");
                assert!(v.get("id").is_none());
                return;
            }
        }
        panic!("did not observe the scoped frame");
    }
}
