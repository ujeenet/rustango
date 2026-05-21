//! Django-shape `m2m_changed` signal — fires when an M2M relationship's
//! junction-table membership changes via [`crate::sql::M2MManager`].
//! Django-parity #410.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signals::m2m::{connect_m2m_changed, M2mChangedContext, M2mAction};
//!
//! connect_m2m_changed(|ctx| Box::pin(async move {
//!     match ctx.action {
//!         M2mAction::Add => tracing::info!(
//!             through = ctx.through,
//!             src = ctx.src_pk,
//!             dst = ctx.dst_pks[0],
//!             "m2m add"
//!         ),
//!         M2mAction::Remove => tracing::info!(
//!             through = ctx.through,
//!             src = ctx.src_pk,
//!             dst = ctx.dst_pks[0],
//!             "m2m remove"
//!         ),
//!         M2mAction::Set => tracing::info!(
//!             through = ctx.through,
//!             count = ctx.dst_pks.len(),
//!             "m2m set"
//!         ),
//!         M2mAction::Clear => tracing::info!(
//!             through = ctx.through,
//!             src = ctx.src_pk,
//!             "m2m clear"
//!         ),
//!     }
//! }));
//! ```
//!
//! ## Action shapes
//!
//! - `Add` — one destination added; `dst_pks = [the_id]`.
//! - `Remove` — one destination removed; `dst_pks = [the_id]`.
//! - `Set` — entire set replaced; `dst_pks = [the new set]` (may be empty).
//! - `Clear` — every destination removed; `dst_pks = []`.
//!
//! Django's `m2m_changed` carries `action = "pre_add" / "post_add" /
//! "pre_remove" / "post_remove" / "pre_clear" / "post_clear"` plus
//! `pk_set`. rustango v1 fires only the `post_*` equivalent (after
//! the SQL completed successfully) — pre_* hooks aren't useful in
//! Rust where you can't abort the operation from the signal handler
//! without convoluted plumbing.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Future returned by m2m-signal receivers. `'static` because the
/// receiver is stored as `Arc<dyn ...>` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque identifier returned by `connect_*` for later use with
/// `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

/// Which M2M mutation triggered the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum M2mAction {
    /// Single destination added via `M2MManager::add_pool`.
    Add,
    /// Single destination removed via `M2MManager::remove_pool`.
    Remove,
    /// Full set replaced via `M2MManager::set_pool`.
    Set,
    /// Every destination removed via `M2MManager::clear_pool`.
    Clear,
}

/// Payload delivered to `m2m_changed` receivers.
#[derive(Debug, Clone)]
pub struct M2mChangedContext {
    pub action: M2mAction,
    /// SQL name of the junction table (e.g. `"post_tags"`).
    pub through: &'static str,
    /// Source-side column name (the FK column pointing at the
    /// source model). Lets receivers disambiguate when one junction
    /// table holds multiple relationships.
    pub src_col: &'static str,
    /// Destination-side column name (the FK column pointing at the
    /// target model).
    pub dst_col: &'static str,
    /// PK value of the source instance whose M2M was mutated.
    pub src_pk: i64,
    /// Affected destination PKs:
    /// - `Add` / `Remove`: single element with the affected id
    /// - `Set`: the new full set (may be empty)
    /// - `Clear`: always empty
    pub dst_pks: Vec<i64>,
}

// ---------------------------------------------------------------- Internal storage

type ReceiverEntry = (ReceiverId, Box<dyn Any + Send + Sync>);
type Bag = Vec<ReceiverEntry>;

fn registry() -> &'static RwLock<HashMap<(), Bag>> {
    static REG: OnceLock<RwLock<HashMap<(), Bag>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_id() -> ReceiverId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    ReceiverId(COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn insert_receiver<R: Any + Send + Sync>(receiver: R) -> ReceiverId {
    let id = next_id();
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.entry(()).or_default().push((id, Box::new(receiver)));
    id
}

fn remove_receiver(id: ReceiverId) -> bool {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get_mut(&()) else {
        return false;
    };
    let before = bag.len();
    bag.retain(|(rid, _)| *rid != id);
    bag.len() != before
}

fn snapshot<R: Any + Send + Sync + Clone>() -> Vec<R> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get(&()) else {
        return Vec::new();
    };
    bag.iter()
        .filter_map(|(_, b)| b.downcast_ref::<R>().cloned())
        .collect()
}

type ChangedReceiver = Arc<dyn Fn(M2mChangedContext) -> ReceiverFuture + Send + Sync>;

// ---------------------------------------------------------------- API

/// Register an `m2m_changed` receiver.
pub fn connect_m2m_changed<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(M2mChangedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: ChangedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(boxed)
}

/// Remove a previously-connected `m2m_changed` receiver.
pub fn disconnect_m2m_changed(id: ReceiverId) -> bool {
    remove_receiver(id)
}

/// Fire `m2m_changed` for `ctx`. Called by [`crate::sql::M2MManager`];
/// available publicly so tests / custom dispatch can also fire.
pub async fn send_m2m_changed(ctx: M2mChangedContext) {
    let receivers: Vec<ChangedReceiver> = snapshot();
    for r in receivers {
        r(ctx.clone()).await;
    }
}

/// Remove **all** m2m-signal receivers. Useful in tests to reset
/// registry state between cases.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Total receivers currently registered.
#[must_use]
pub fn receiver_count() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.get(&()).map_or(0, Vec::len)
}
