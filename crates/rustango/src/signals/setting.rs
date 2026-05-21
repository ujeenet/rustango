//! Django-shape `setting_changed` signal — fires when a
//! [`crate::test_settings::with_overridden`] override scope is
//! entered or left. Django-parity #415.
//!
//! Django's `setting_changed` fires per-setting with `setting`,
//! `value`, `enter` (bool). rustango's overlay scopes are
//! whole-Settings replacements rather than per-field deltas, so the
//! signal carries just `enter: bool` — `true` when the scope begins,
//! `false` when it ends. Receivers typically use it to invalidate
//! caches that depend on configuration values.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signals::setting::{connect_setting_changed, SettingChangedContext};
//!
//! connect_setting_changed(|ctx| Box::pin(async move {
//!     if ctx.enter {
//!         tracing::debug!("settings overlay entered — flushing caches");
//!         // ...
//!     } else {
//!         tracing::debug!("settings overlay left");
//!     }
//! }));
//! ```

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

/// Payload delivered to `setting_changed` receivers.
#[derive(Debug, Clone, Copy)]
pub struct SettingChangedContext {
    /// `true` when an override scope is entered; `false` when the
    /// scope's future completes and the scope is leaving.
    pub enter: bool,
}

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

type ChangedReceiver = Arc<dyn Fn(SettingChangedContext) -> ReceiverFuture + Send + Sync>;

/// Register a `setting_changed` receiver.
pub fn connect_setting_changed<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(SettingChangedContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: ChangedReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(boxed)
}

/// Remove a previously-connected `setting_changed` receiver.
pub fn disconnect_setting_changed(id: ReceiverId) -> bool {
    remove_receiver(id)
}

/// Fire `setting_changed` for `ctx`. Called by
/// [`crate::test_settings::with_overridden`]; available publicly so
/// custom configuration-overlay machinery can dispatch the same
/// signal.
pub async fn send_setting_changed(ctx: SettingChangedContext) {
    let receivers: Vec<ChangedReceiver> = snapshot();
    for r in receivers {
        r(ctx).await;
    }
}

/// Remove **all** setting-signal receivers. Test maintenance helper.
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
