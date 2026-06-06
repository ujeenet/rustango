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
/// receiver in registration order. Becomes a no-op inside
/// [`without_signals`] / [`save_quietly`] scopes (issue #827).
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

/// Fire the `post_save` signal for `instance`. No-op inside
/// [`without_signals`] / [`save_quietly`].
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

/// Fire the `pre_delete` signal for `instance`. No-op inside
/// [`without_signals`] / [`delete_quietly`].
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

/// Fire the `post_delete` signal for `instance`. No-op inside
/// [`without_signals`] / [`delete_quietly`].
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

// ------------------------------------------------------------------ Quiet writes (#827)
//
// Eloquent shape: `Model::withoutEvents(fn)` + `saveQuietly()` /
// `deleteQuietly()`. Rust shape: a task-local boolean toggled by the
// `without_signals` / `save_quietly` / `delete_quietly` async scope
// helpers. Each `send_*` checks the flag and early-returns when set.
//
// Storage is `tokio::task_local!` rather than thread-local so an
// async runtime that moves tasks between worker threads (default
// multi-threaded runtime) still observes the right scope. The
// helpers are async — they `await` the user closure inside the
// task-local scope.
//
// Nested scopes compose: every entry pushes `true`; the scope's
// outer state on exit is restored.

tokio::task_local! {
    static SUPPRESS_SIGNALS: bool;
}

/// `true` when the current async task is executing inside a
/// [`without_signals`] / [`save_quietly`] / [`delete_quietly`] scope.
/// All `send_*` functions early-return when this is `true`.
fn signals_suppressed() -> bool {
    SUPPRESS_SIGNALS.try_with(|v| *v).unwrap_or(false)
}

/// Suppress every `send_*` dispatch (pre/post save + pre/post delete)
/// while awaiting `fut`. Eloquent's `Model::withoutEvents(fn)` /
/// Django's manual signal-bypass pattern — useful when bulk-loading
/// fixtures, running a migration that touches model rows, or
/// performing internal bookkeeping that shouldn't trip side-effecty
/// receivers.
///
/// Nested calls compose — the innermost scope sets the flag; on exit
/// the surrounding state is restored.
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

/// Sugar for [`without_signals`] — name matches Eloquent's
/// `saveQuietly()`. The actual save call is the caller's; this just
/// wraps it in the suppression scope.
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

/// Sugar for [`without_signals`] — name matches Eloquent's
/// `deleteQuietly()`.
pub async fn delete_quietly<F, R>(fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    without_signals(fut).await
}

// ------------------------------------------------------------------ Observer<T> (#827)
//
// Eloquent shape: one struct implements `Observer` with default-noop
// methods; `Model::observe(MyObserver)` wires all four signals at
// once. Rust shape: an `Observer<T>` trait with four async default
// methods + a free `observe::<T>(obs)` function that connects each
// to the registry and returns a bundle of `ReceiverId`s so the user
// can later detach the whole observer with a single
// `disconnect_observer` call.
//
// The four methods take `Arc<T>` (same as the underlying
// `connect_*` shape) so an observer impl can store state on the
// struct and read it without holding locks.

/// Bundle of [`ReceiverId`]s returned by [`observe`]. Pass to
/// [`disconnect_observer`] to detach every wired hook in one call.
#[derive(Debug, Clone)]
pub struct ObserverHandle {
    pub pre_save: ReceiverId,
    pub post_save: ReceiverId,
    pub pre_delete: ReceiverId,
    pub post_delete: ReceiverId,
}

/// Eloquent-style observer trait — group all four lifecycle hooks
/// for a model under a single struct. Every method has a no-op
/// default; implementors override only the events they care about.
///
/// Methods take `Arc<T>` (matching the underlying `connect_*`
/// signatures) so an observer can hold state without lifetime
/// gymnastics. Implementors must be `Send + Sync + 'static` — the
/// observer is stored behind an `Arc` in the global registry.
///
/// Wire with [`observe`]; detach with [`disconnect_observer`]. Issue #827.
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
    /// Fired before INSERT or UPDATE. Default is a no-op.
    fn pre_save(&self, _instance: Arc<T>) -> ReceiverFuture {
        Box::pin(async {})
    }
    /// Fired after a successful save. `ctx.created` distinguishes
    /// INSERT from UPDATE. Default is a no-op.
    fn post_save(&self, _instance: Arc<T>, _ctx: PostSaveContext) -> ReceiverFuture {
        Box::pin(async {})
    }
    /// Fired before DELETE. Default is a no-op.
    fn pre_delete(&self, _instance: Arc<T>) -> ReceiverFuture {
        Box::pin(async {})
    }
    /// Fired after DELETE. Default is a no-op.
    fn post_delete(&self, _instance: Arc<T>) -> ReceiverFuture {
        Box::pin(async {})
    }
}

/// Wire every method of `obs` to its corresponding signal for model
/// `T`. Returns an [`ObserverHandle`] carrying the four
/// [`ReceiverId`]s; pass to [`disconnect_observer`] when you no
/// longer want the hooks. Issue #827.
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

/// Detach every receiver wired by a previous [`observe`] call.
/// Returns the count of hooks actually removed (0–4; less than 4
/// means some had been individually disconnected already).
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
