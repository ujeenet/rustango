//! Migration signals: `pre_migrate` and `post_migrate`.
//!
//! Receivers register globally and run around two migrate paths:
//!
//! - [`crate::migrate::apply_all`] and
//!   [`crate::migrate::apply_all_pool`], the bootstrap CREATE TABLE
//!   walk
//! - [`crate::migrate::migrate`], the file-based migrations
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signals::migrate::{
//!     connect_pre_migrate, connect_post_migrate,
//!     PreMigrateContext, PostMigrateContext,
//! };
//!
//! connect_pre_migrate(|ctx| Box::pin(async move {
//!     tracing::info!(source = ctx.source, "migrate starting");
//! }));
//! connect_post_migrate(|ctx| Box::pin(async move {
//!     tracing::info!(
//!         source = ctx.source,
//!         applied = ctx.applied.len(),
//!         "migrate finished"
//!     );
//! }));
//! ```
//!
//! ## When each one is sent
//!
//! - `pre_migrate`: once, before anything is applied. `source` names
//!   the entry point, such as `"apply_all"` or `"migrate"`.
//! - `post_migrate`: once, after the path finishes without error.
//!   `applied` lists the migration names that ran, in order. It is
//!   empty for the bootstrap walk, which has no per-migration names.
//!
//! Receivers run one at a time, in registration order, each with its
//! own clone of the context. To run work in parallel, or to keep a
//! panic from stopping the chain, use `tokio::spawn`.

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

// ---------------------------------------------------------------- Context types

/// What a `pre_migrate` receiver gets, once before the run starts.
#[derive(Debug, Clone)]
pub struct PreMigrateContext {
    /// Which entry point sent the signal: `"apply_all"` or
    /// `"apply_all_pool"` for the bootstrap walk, `"migrate"` for
    /// file-based migrations. One receiver can branch on it.
    pub source: &'static str,
}

/// What a `post_migrate` receiver gets, once the run has succeeded.
#[derive(Debug, Clone)]
pub struct PostMigrateContext {
    pub source: &'static str,
    /// Migrations that ran, in apply order. Empty for the bootstrap
    /// walk, which has no per-migration names.
    pub applied: Vec<String>,
}

// ---------------------------------------------------------------- Internal storage

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SignalKind {
    PreMigrate,
    PostMigrate,
}

type ReceiverEntry = (ReceiverId, Box<dyn Any + Send + Sync>);
type Bag = Vec<ReceiverEntry>;

fn registry() -> &'static RwLock<HashMap<SignalKind, Bag>> {
    static REG: OnceLock<RwLock<HashMap<SignalKind, Bag>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_id() -> ReceiverId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    ReceiverId(COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn insert_receiver<R: Any + Send + Sync>(kind: SignalKind, receiver: R) -> ReceiverId {
    let id = next_id();
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.entry(kind).or_default().push((id, Box::new(receiver)));
    id
}

fn remove_receiver(kind: SignalKind, id: ReceiverId) -> bool {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get_mut(&kind) else {
        return false;
    };
    let before = bag.len();
    bag.retain(|(rid, _)| *rid != id);
    bag.len() != before
}

fn snapshot<R: Any + Send + Sync + Clone>(kind: SignalKind) -> Vec<R> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let Some(bag) = reg.get(&kind) else {
        return Vec::new();
    };
    bag.iter()
        .filter_map(|(_, b)| b.downcast_ref::<R>().cloned())
        .collect()
}

// ---------------------------------------------------------------- Receiver type aliases

type PreReceiver = Arc<dyn Fn(PreMigrateContext) -> ReceiverFuture + Send + Sync>;
type PostReceiver = Arc<dyn Fn(PostMigrateContext) -> ReceiverFuture + Send + Sync>;

// ---------------------------------------------------------------- pre_migrate

/// Register a `pre_migrate` receiver.
pub fn connect_pre_migrate<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(PreMigrateContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: PreReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::PreMigrate, boxed)
}

/// Remove a `pre_migrate` receiver.
pub fn disconnect_pre_migrate(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::PreMigrate, id)
}

/// Send `pre_migrate`. The framework's migrate paths call this; it is
/// public so tests and custom runners can too.
pub async fn send_pre_migrate(ctx: PreMigrateContext) {
    let receivers: Vec<PreReceiver> = snapshot(SignalKind::PreMigrate);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- post_migrate

/// Register a `post_migrate` receiver.
pub fn connect_post_migrate<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(PostMigrateContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let boxed: PostReceiver = Arc::new(move |ctx| Box::pin(receiver(ctx)));
    insert_receiver(SignalKind::PostMigrate, boxed)
}

/// Remove a `post_migrate` receiver.
pub fn disconnect_post_migrate(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::PostMigrate, id)
}

/// Send `post_migrate` for `ctx`.
pub async fn send_post_migrate(ctx: PostMigrateContext) {
    let receivers: Vec<PostReceiver> = snapshot(SignalKind::PostMigrate);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Maintenance

/// Remove every migrate-signal receiver. Mostly for resetting state
/// between tests.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// How many receivers are registered across both migrate signals.
/// Mostly useful in tests.
pub fn receiver_count() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    [SignalKind::PreMigrate, SignalKind::PostMigrate]
        .iter()
        .map(|kind| reg.get(kind).map_or(0, Vec::len))
        .sum()
}
