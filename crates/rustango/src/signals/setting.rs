//! The `setting_changed` signal. It is sent when a
//! [`crate::test_settings::with_overridden`] scope is entered or
//! left.
//!
//! Django sends one per setting, with a name and a value. Here an
//! overlay replaces the whole `Settings`, so the signal carries only
//! `enter`: `true` on the way in, `false` on the way out. Receivers
//! mostly use it to drop caches that depend on config.
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

/// What a `setting_changed` receiver gets.
#[derive(Debug, Clone, Copy)]
pub struct SettingChangedContext {
    /// `true` when a scope is entered, `false` when it ends.
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

/// Remove a `setting_changed` receiver.
pub fn disconnect_setting_changed(id: ReceiverId) -> bool {
    remove_receiver(id)
}

/// Send `setting_changed`. [`crate::test_settings::with_overridden`]
/// calls this; it is public so your own overlay code can too.
pub async fn send_setting_changed(ctx: SettingChangedContext) {
    let receivers: Vec<ChangedReceiver> = snapshot();
    for r in receivers {
        r(ctx).await;
    }
}

/// Remove every `setting_changed` receiver. Mostly for tests.
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
