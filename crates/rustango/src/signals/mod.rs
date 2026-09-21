//! Model signals in Django's shape: `pre_save`, `post_save`,
//! `pre_delete`, `post_delete`.
//!
//! Receivers are registered globally per model type. When a signal is
//! sent, they run one after another.
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
//! // Saving does not send the signal for you — send it yourself:
//! post.save_on(&pool).await?;
//! send_post_save(&post, PostSaveContext { created: true }).await;
//! ```
//!
//! ## Available signals
//!
//! | Signal | Receiver signature | Send it |
//! |--------|---------------------|----------|
//! | `pre_save` | `Fn(Arc<T>) -> Future` | Before an INSERT or UPDATE |
//! | `post_save` | `Fn(Arc<T>, PostSaveContext) -> Future` | After an INSERT or UPDATE |
//! | `pre_delete` | `Fn(Arc<T>) -> Future` | Before a DELETE |
//! | `post_delete` | `Fn(Arc<T>) -> Future` | After a DELETE |
//!
//! ## HTTP request lifecycle
//!
//! `request_started`, `request_finished` and `got_request_exception`
//! live in [`request`], with their own registry and their own
//! connect, disconnect and send functions. The
//! [`request::RequestSignalsLayer`] tower layer sends them around
//! every axum request, so those you do not send by hand.
//!
//! ## Rules
//!
//! - Receivers run one at a time, in registration order.
//! - Each gets an `Arc<T>` clone of the instance, so there are no
//!   borrow lifetimes to work around. That is why `T: Clone` is
//!   required.
//! - `connect_*` returns a `ReceiverId` for a later `disconnect_*`.
//! - **A receiver must not panic.** A panic stops the rest of the
//!   chain and reaches whoever called `send_*`. For isolation, run
//!   the body in `tokio::spawn`.
//!
//! [`request`]: crate::signals::request
//! [`request::RequestSignalsLayer`]: crate::signals::request::RequestSignalsLayer

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

/// The future a receiver returns. It is `'static` because the
/// receiver is boxed and may run after the caller has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Handle returned by `connect_*`, for a later `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

/// Tells a `post_save` receiver whether the row was inserted or
/// updated.
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

/// Copy the receivers for `key` out of the registry, so the lock is
/// released before any of them is awaited.
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

/// Receiver for `pre_save`, `pre_delete` and `post_delete`: it takes
/// only the model.
type SimpleReceiver<T> = Arc<dyn Fn(Arc<T>) -> ReceiverFuture + Send + Sync>;

/// Receiver for `post_save`: it also takes a `PostSaveContext`.
type PostSaveReceiver<T> = Arc<dyn Fn(Arc<T>, PostSaveContext) -> ReceiverFuture + Send + Sync>;

// ------------------------------------------------------------------ pre_save

/// Register a `pre_save` receiver for `T`. It gets an `Arc<T>` copy
/// of the instance. Returns an id for [`disconnect_pre_save`].
pub fn connect_pre_save<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: SimpleReceiver<T> = Arc::new(move |instance| Box::pin(receiver(instance)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PreSave), boxed)
}

/// Remove a `pre_save` receiver. `true` when one was removed.
pub fn disconnect_pre_save<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PreSave), id)
}

/// Send `pre_save` for `instance`, awaiting each receiver in
/// registration order. Does nothing inside a [`without_signals`] or
/// [`save_quietly`] scope.
pub async fn send_pre_save<T: Model + Clone + 'static>(instance: &T) {
    if signals_suppressed() {
        return;
    }
    let receivers: Vec<SimpleReceiver<T>> =
        snapshot::<SimpleReceiver<T>>((TypeId::of::<T>(), SignalKind::PreSave));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone()).await;
    }
}

// ------------------------------------------------------------------ post_save

/// Register a `post_save` receiver for `T`. It gets an `Arc<T>` of
/// the instance and a [`PostSaveContext`], whose `created` field is
/// `true` for an insert.
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

/// Remove a `post_save` receiver.
pub fn disconnect_post_save<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PostSave), id)
}

/// Send `post_save` for `instance`. Does nothing inside
/// [`without_signals`] or [`save_quietly`].
pub async fn send_post_save<T: Model + Clone + 'static>(instance: &T, ctx: PostSaveContext) {
    if signals_suppressed() {
        return;
    }
    let receivers: Vec<PostSaveReceiver<T>> =
        snapshot::<PostSaveReceiver<T>>((TypeId::of::<T>(), SignalKind::PostSave));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone(), ctx).await;
    }
}

// ------------------------------------------------------------------ pre_delete

/// Register a `pre_delete` receiver for `T`.
pub fn connect_pre_delete<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: SimpleReceiver<T> = Arc::new(move |instance| Box::pin(receiver(instance)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PreDelete), boxed)
}

/// Remove a `pre_delete` receiver.
pub fn disconnect_pre_delete<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PreDelete), id)
}

/// Send `pre_delete` for `instance`. Does nothing inside
/// [`without_signals`] or [`delete_quietly`].
pub async fn send_pre_delete<T: Model + Clone + 'static>(instance: &T) {
    if signals_suppressed() {
        return;
    }
    let receivers: Vec<SimpleReceiver<T>> =
        snapshot::<SimpleReceiver<T>>((TypeId::of::<T>(), SignalKind::PreDelete));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone()).await;
    }
}

// ------------------------------------------------------------------ post_delete

/// Register a `post_delete` receiver for `T`.
pub fn connect_post_delete<T, F, Fut>(receiver: F) -> ReceiverId
where
    T: Model + Clone + 'static,
    F: Fn(Arc<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: SimpleReceiver<T> = Arc::new(move |instance| Box::pin(receiver(instance)));
    insert_receiver((TypeId::of::<T>(), SignalKind::PostDelete), boxed)
}

/// Remove a `post_delete` receiver.
pub fn disconnect_post_delete<T: Model + 'static>(id: ReceiverId) -> bool {
    remove_receiver((TypeId::of::<T>(), SignalKind::PostDelete), id)
}

/// Send `post_delete` for `instance`. Does nothing inside
/// [`without_signals`] or [`delete_quietly`].
pub async fn send_post_delete<T: Model + Clone + 'static>(instance: &T) {
    if signals_suppressed() {
        return;
    }
    let receivers: Vec<SimpleReceiver<T>> =
        snapshot::<SimpleReceiver<T>>((TypeId::of::<T>(), SignalKind::PostDelete));
    let arc = Arc::new(instance.clone());
    for r in receivers {
        r(arc.clone()).await;
    }
}

// ------------------------------------------------------------------ Maintenance

/// Remove every receiver, for every model and every signal. Mostly
/// for resetting state between tests.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// How many receivers are registered for `T` across all signals.
/// Mostly useful in tests.
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

// ------------------------------------------------------------------ Quiet writes
//
// A task-local boolean, set by the `without_signals`,
// `save_quietly` and `delete_quietly` scope helpers. Every `send_*`
// checks it and returns early when it is set.
//
// It is a `tokio::task_local!`, not a thread-local, so the scope
// still holds when the runtime moves a task between worker threads.
// Scopes nest: the outer state is restored on exit.

tokio::task_local! {
    static SUPPRESS_SIGNALS: bool;
}

/// `true` when the current task is inside a [`without_signals`],
/// [`save_quietly`] or [`delete_quietly`] scope.
fn signals_suppressed() -> bool {
    SUPPRESS_SIGNALS.try_with(|v| *v).unwrap_or(false)
}

/// Turn off every `send_*` while awaiting `fut`. Use it when loading
/// fixtures in bulk, running a migration that touches rows, or doing
/// internal bookkeeping that should not trigger receivers.
///
/// Calls nest; the outer state comes back on exit.
///
/// ```ignore
/// use rustango::signals::without_signals;
///
/// without_signals(async {
///     for row in batch {
///         row.save_pool(&pool).await?;
///     }
///     Ok::<_, ExecError>(())
/// }).await?;
/// ```
pub async fn without_signals<F, R>(fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    SUPPRESS_SIGNALS.scope(true, fut).await
}

/// [`without_signals`] under a name that reads better around a save.
/// You still make the save call; this only wraps it.
///
/// ```ignore
/// signals::save_quietly(post.save_pool(&pool)).await?;
/// ```
pub async fn save_quietly<F, R>(fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    without_signals(fut).await
}

/// [`without_signals`] under a name that reads better around a
/// delete.
pub async fn delete_quietly<F, R>(fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    without_signals(fut).await
}

// ------------------------------------------------------------------ Observer<T>
//
// One struct can carry all four hooks for a model. `observe::<T>`
// registers each of them and hands back the ids, so
// `disconnect_observer` can remove them all at once.

/// The four [`ReceiverId`]s from one [`observe`] call. Pass it to
/// [`disconnect_observer`] to remove every hook at once.
#[derive(Debug, Clone)]
pub struct ObserverHandle {
    pub pre_save: ReceiverId,
    pub post_save: ReceiverId,
    pub pre_delete: ReceiverId,
    pub post_delete: ReceiverId,
}

/// Groups all four model hooks in one struct. Every method does
/// nothing by default, so you override only the ones you need.
///
/// The methods take `Arc<T>`, so an observer can hold state without
/// lifetime trouble. An implementor must be `Send + Sync + 'static`,
/// because the registry keeps it behind an `Arc`.
///
/// Register with [`observe`], remove with [`disconnect_observer`].
///
/// ```ignore
/// struct AuditLog;
/// impl rustango::signals::Observer<Post> for AuditLog {
///     async fn post_save(&self, post: std::sync::Arc<Post>, ctx: rustango::signals::PostSaveContext) {
///         tracing::info!(?ctx.created, post_id = ?post.id, "post saved");
///     }
/// }
/// let handle = rustango::signals::observe::<Post, _>(AuditLog);
/// // ...
/// rustango::signals::disconnect_observer::<Post>(&handle);
/// ```
pub trait Observer<T: Model + Clone + 'static>: Send + Sync + 'static {
    /// Runs before an INSERT or UPDATE. Does nothing by default.
    fn pre_save(&self, _instance: Arc<T>) -> ReceiverFuture {
        Box::pin(async {})
    }
    /// Runs after a save. `ctx.created` tells an insert from an
    /// update. Does nothing by default.
    fn post_save(&self, _instance: Arc<T>, _ctx: PostSaveContext) -> ReceiverFuture {
        Box::pin(async {})
    }
    /// Runs before a DELETE. Does nothing by default.
    fn pre_delete(&self, _instance: Arc<T>) -> ReceiverFuture {
        Box::pin(async {})
    }
    /// Runs after a DELETE. Does nothing by default.
    fn post_delete(&self, _instance: Arc<T>) -> ReceiverFuture {
        Box::pin(async {})
    }
}

/// Register each method of `obs` as a receiver for model `T`.
/// Returns an [`ObserverHandle`] for [`disconnect_observer`].
pub fn observe<T, O>(obs: O) -> ObserverHandle
where
    T: Model + Clone + 'static,
    O: Observer<T>,
{
    let obs = Arc::new(obs);
    let o1 = Arc::clone(&obs);
    let o2 = Arc::clone(&obs);
    let o3 = Arc::clone(&obs);
    let o4 = Arc::clone(&obs);
    ObserverHandle {
        pre_save: connect_pre_save::<T, _, _>(move |i| {
            let o = Arc::clone(&o1);
            async move { o.pre_save(i).await }
        }),
        post_save: connect_post_save::<T, _, _>(move |i, ctx| {
            let o = Arc::clone(&o2);
            async move { o.post_save(i, ctx).await }
        }),
        pre_delete: connect_pre_delete::<T, _, _>(move |i| {
            let o = Arc::clone(&o3);
            async move { o.pre_delete(i).await }
        }),
        post_delete: connect_post_delete::<T, _, _>(move |i| {
            let o = Arc::clone(&o4);
            async move { o.post_delete(i).await }
        }),
    }
}

/// Remove every receiver an [`observe`] call registered. Returns how
/// many went away. Fewer than four means some were already removed.
pub fn disconnect_observer<T: Model + 'static>(handle: &ObserverHandle) -> usize {
    [
        disconnect_pre_save::<T>(handle.pre_save),
        disconnect_post_save::<T>(handle.post_save),
        disconnect_pre_delete::<T>(handle.pre_delete),
        disconnect_post_delete::<T>(handle.post_delete),
    ]
    .into_iter()
    .filter(|r| *r)
    .count()
}
