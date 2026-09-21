//! The `m2m_changed` signal, in Django's shape. It is sent when
//! [`crate::sql::M2MManager`] changes what is in a junction table.
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
//! ## What each action carries
//!
//! - `Add`: one destination added, `dst_pks` holds its id.
//! - `Remove`: one destination removed, `dst_pks` holds its id.
//! - `Set`: the whole set replaced, `dst_pks` is the new set, which
//!   may be empty.
//! - `Clear`: everything removed, `dst_pks` is empty.
//!
//! Django also has `pre_add`, `pre_remove` and `pre_clear`. Only the
//! `post_*` case exists here, sent after the SQL succeeds, because a
//! Rust receiver cannot cancel the operation anyway.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// The future a receiver returns. It is `'static` because the
/// receiver is stored behind an `Arc` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Handle returned by `connect_*`, for a later `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

/// Which change caused the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum M2mAction {
    /// One destination added, through `M2MManager::add_pool`.
    Add,
    /// One destination removed, through `M2MManager::remove_pool`.
    Remove,
    /// The whole set replaced, through `M2MManager::set_pool`.
    Set,
    /// Every destination removed, through `M2MManager::clear_pool`.
    Clear,
}

/// What an `m2m_changed` receiver gets.
#[derive(Debug, Clone)]
pub struct M2mChangedContext {
    pub action: M2mAction,
    /// Name of the junction table, such as `"post_tags"`.
    pub through: &'static str,
    /// The column pointing at the source model. It tells relations
    /// apart when one junction table holds several.
    pub src_col: &'static str,
    /// The column pointing at the target model.
    pub dst_col: &'static str,
    /// Primary key of the source row that changed.
    pub src_pk: i64,
    /// The destination keys involved. One id for `Add` and
    /// `Remove`, the new set for `Set`, and empty for `Clear`.
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

/// Remove an `m2m_changed` receiver.
pub fn disconnect_m2m_changed(id: ReceiverId) -> bool {
    remove_receiver(id)
}

/// Send `m2m_changed`. [`crate::sql::M2MManager`] calls this; it is
/// public so tests and custom dispatch can too.
pub async fn send_m2m_changed(ctx: M2mChangedContext) {
    let receivers: Vec<ChangedReceiver> = snapshot();
    for r in receivers {
        r(ctx.clone()).await;
    }
}

/// Remove every `m2m_changed` receiver. Mostly for resetting state
/// between tests.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// How many receivers are registered.
#[must_use]
pub fn receiver_count() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.get(&()).map_or(0, Vec::len)
}
