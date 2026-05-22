//! Django-shape model signals — `pre_save`, `post_save`, `pre_delete`, `post_delete`.
//!
//! Receivers register globally per model type and run sequentially when the
//! corresponding signal is fired by a write path.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signals::{connect_post_save, send_post_save, PostSaveContext};
//!
//! // Register a receiver at startup:
//! connect_post_save::<Post>(|post, ctx| Box::pin(async move {
//!     if ctx.created {
//!         tracing::info!("New post #{}", post.id.get().copied().unwrap_or(0));
//!     }
//! }));
//!
//! // Fire after your save call (or wire into the macro-generated save() in a follow-up slice):
//! post.save_on(&pool).await?;
//! send_post_save(&post, PostSaveContext { created: true }).await;
//! ```
//!
//! ## Available signals
//!
//! | Signal | Receiver signature | Fired by |
//! |--------|---------------------|----------|
//! | `pre_save` | `Fn(Arc<T>) -> Future` | Before INSERT or UPDATE |
//! | `post_save` | `Fn(Arc<T>, PostSaveContext) -> Future` | After INSERT or UPDATE |
//! | `pre_delete` | `Fn(Arc<T>) -> Future` | Before DELETE |
//! | `post_delete` | `Fn(Arc<T>) -> Future` | After DELETE |
//!
//! ## HTTP request lifecycle
//!
//! Request-level signals (`request_started` / `request_finished` /
//! `got_request_exception`) live in [`request`] — separate registry,
//! separate connect/disconnect/send functions, plus a
//! [`request::RequestSignalsLayer`] tower layer that fires them
//! around every axum request. Issue #53.
//!
//! ## Semantics
//!
//! - Receivers run **sequentially** in registration order, awaited one at a time.
//! - Each receiver gets an `Arc<T>` clone of the instance — no borrow lifetimes.
//! - `T: Clone + 'static` is required so the dispatcher can wrap into `Arc`.
//! - `connect_*` returns a `ReceiverId` you can pass to `disconnect_*` later.
//! - **Receivers must not panic.** A panicking receiver aborts the rest of the
//!   dispatch chain and propagates up to the caller of `send_*`. If you need
//!   isolation, wrap your receiver body in `tokio::spawn`.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};

use crate::core::Model;

pub mod admin;
pub mod auth;
pub mod m2m;
pub mod migrate;
pub mod request;
pub mod setting;

/// Future returned by signal receivers. `'static` because the receiver
/// is stored as `Box<dyn ...>` and may run after the caller has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque identifier returned by `connect_*` for later use with `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

/// Context passed to `post_save` receivers — distinguishes INSERT from UPDATE.
#[derive(Debug, Clone, Copy)]
pub struct PostSaveContext {
    /// `true` when the row was newly inserted; `false` for updates.
    pub created: bool,
}

// ------------------------------------------------------------------ Internal storage

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SignalKind {
    PreSave,
    PostSave,
    PreDelete,
    PostDelete,
}

type ReceiverEntry = (ReceiverId, Box<dyn Any + Send + Sync>);
type Bag = Vec<ReceiverEntry>;

fn registry() -> &'static RwLock<HashMap<(TypeId, SignalKind), Bag>> {
    static REG: OnceLock<RwLock<HashMap<(TypeId, SignalKind), Bag>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_id() -> ReceiverId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    ReceiverId(COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn insert_receiver<R: Any + Send + Sync>(key: (TypeId, SignalKind), receiver: R) -> ReceiverId {
    let id = next_id();
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.entry(key).or_default().push((id, Box::new(receiver)));
    id
}

fn remove_receiver(key: (TypeId, SignalKind), id: ReceiverId) -> bool {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get_mut(&key) else {
        return false;
    };
    let before = bag.len();
    bag.retain(|(rid, _)| *rid != id);
    bag.len() != before
}

/// Snapshot the receivers for `key` into a `Vec<Arc<R>>` so dispatch
/// can release the registry lock immediately, avoiding holding it
/// across await points.
fn snapshot<R: Any + Send + Sync + Clone>(key: (TypeId, SignalKind)) -> Vec<R> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get(&key) else {
        return Vec::new();
    };
    bag.iter()
        .filter_map(|(_, b)| b.downcast_ref::<R>().cloned())
        .collect()
}

// ------------------------------------------------------------------ Receiver type aliases

/// `pre_save` / `pre_delete` / `post_delete` receiver — takes the model only.
type SimpleReceiver<T> = Arc<dyn Fn(Arc<T>) -> ReceiverFuture + Send + Sync>;

/// `post_save` receiver — takes model + `PostSaveContext`.
type PostSaveReceiver<T> = Arc<dyn Fn(Arc<T>, PostSaveContext) -> ReceiverFuture + Send + Sync>;

// ------------------------------------------------------------------ pre_save

/// Register a `pre_save` receiver for type `T`.
///
/// The receiver runs before every `save()` for `T`. It receives an
/// `Arc<T>` snapshot of the instance.
///
/// Returns a [`ReceiverId`] for later [`disconnect_pre_save`].
pub fn connect_pre_save<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: SimpleReceiver<T> = Arc::new(move |instance| Box::pin(receiver(instance)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PreSave), boxed)
}

/// Remove a previously connected `pre_save` receiver. Returns `true`
/// when an entry was removed.
pub fn disconnect_pre_save<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PreSave), id)
}

/// Fire the `pre_save` signal for `instance`. Awaits every connected
/// receiver in registration order.
pub async fn send_pre_save<T: Model + Clone + 'static>(instance: &T) {
    let receivers: Vec<SimpleReceiver<T>> =
        snapshot::<SimpleReceiver<T>>((TypeId::of::<T>(), SignalKind::PreSave));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone()).await;
    }
}

// ------------------------------------------------------------------ post_save

/// Register a `post_save` receiver for type `T`.
///
/// The receiver runs after every successful `save()`. It receives an
/// `Arc<T>` of the instance and a [`PostSaveContext`] indicating
/// whether the save was an insert (`created = true`) or update.
pub fn connect_post_save<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>, PostSaveContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: PostSaveReceiver<T> =
        Arc::new(move |instance, ctx| Box::pin(receiver(instance, ctx)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PostSave), boxed)
}

/// Remove a previously connected `post_save` receiver.
pub fn disconnect_post_save<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PostSave), id)
}

/// Fire the `post_save` signal for `instance`.
pub async fn send_post_save<T: Model + Clone + 'static>(instance: &T, ctx: PostSaveContext) {
    let receivers: Vec<PostSaveReceiver<T>> =
        snapshot::<PostSaveReceiver<T>>((TypeId::of::<T>(), SignalKind::PostSave));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone(), ctx).await;
    }
}

// ------------------------------------------------------------------ pre_delete

/// Register a `pre_delete` receiver for type `T`.
pub fn connect_pre_delete<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: SimpleReceiver<T> = Arc::new(move |instance| Box::pin(receiver(instance)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PreDelete), boxed)
}

/// Remove a previously connected `pre_delete` receiver.
pub fn disconnect_pre_delete<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PreDelete), id)
}

/// Fire the `pre_delete` signal for `instance`.
pub async fn send_pre_delete<T: Model + Clone + 'static>(instance: &T) {
    let receivers: Vec<SimpleReceiver<T>> =
        snapshot::<SimpleReceiver<T>>((TypeId::of::<T>(), SignalKind::PreDelete));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone()).await;
    }
}

// ------------------------------------------------------------------ post_delete

/// Register a `post_delete` receiver for type `T`.
pub fn connect_post_delete<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: SimpleReceiver<T> = Arc::new(move |instance| Box::pin(receiver(instance)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PostDelete), boxed)
}

/// Remove a previously connected `post_delete` receiver.
pub fn disconnect_post_delete<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PostDelete), id)
}

/// Fire the `post_delete` signal for `instance`.
pub async fn send_post_delete<T: Model + Clone + 'static>(instance: &T) {
    let receivers: Vec<SimpleReceiver<T>> =
        snapshot::<SimpleReceiver<T>>((TypeId::of::<T>(), SignalKind::PostDelete));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone()).await;
    }
}

// ------------------------------------------------------------------ Maintenance

/// Remove **all** receivers for **all** model types and signal kinds.
///
/// Useful in tests to reset registry state between cases. Production
/// code rarely needs this.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Number of currently registered receivers across all signals for `T`.
/// Useful in tests to assert connection state.
pub fn receiver_count<T: Model + 'static>() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let id = TypeId::of::<T>();
    [
        SignalKind::PreSave,
        SignalKind::PostSave,
        SignalKind::PreDelete,
        SignalKind::PostDelete,
    ]
    .iter()
    .map(|kind| reg.get(&(id, *kind)).map_or(0, Vec::len))
    .sum()
}
