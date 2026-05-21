//! Django-shape migration lifecycle signals — `pre_migrate` /
//! `post_migrate`. Django-parity #411.
//!
//! Receivers register globally and fire around the framework's
//! migrate paths:
//!
//! - [`crate::migrate::apply_all`] / [`crate::migrate::apply_all_pool`]
//!   (bootstrap CREATE TABLE walk)
//! - [`crate::migrate::migrate`] (file-based ledger-tracked migrations)
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
//! ## Where each signal fires
//!
//! - `pre_migrate` — once before the framework starts applying
//!   migrations / CREATE TABLEs for a session. `source` identifies
//!   the entry point (`"apply_all"`, `"migrate"`, …).
//! - `post_migrate` — once after the migrate path completes
//!   successfully. `applied` is the ordered list of migration names
//!   that ran during this invocation (empty for `apply_all` /
//!   `apply_all_pool` since those don't carry per-migration names).
//!
//! ## Semantics
//!
//! - Receivers run **sequentially** in registration order, awaited one
//!   at a time. Wrap in `tokio::spawn` for fanout.
//! - A panicking receiver aborts the dispatch chain and propagates.
//! - Each receiver gets a `Clone`d context — no borrow lifetimes.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Future returned by migrate-signal receivers. `'static` because the
/// receiver is stored as `Arc<dyn ...>` and may run after the caller
/// has returned.
pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque identifier returned by `connect_*` for later use with
/// `disconnect_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

// ---------------------------------------------------------------- Context types

/// Payload delivered to `pre_migrate` receivers — fires once before
/// the framework's migrate path starts.
#[derive(Debug, Clone)]
pub struct PreMigrateContext {
    /// Free-form identifier for the entry point that fired the
    /// signal: `"apply_all"` (no-file bootstrap), `"apply_all_pool"`
    /// (tri-dialect bootstrap), `"migrate"` (file-based ledger). Lets
    /// a single receiver discriminate between modes if it cares.
    pub source: &'static str,
}

/// Payload delivered to `post_migrate` receivers — fires once after
/// the framework's migrate path completes successfully.
#[derive(Debug, Clone)]
pub struct PostMigrateContext {
    pub source: &'static str,
    /// Names of migrations that ran during this invocation, in apply
    /// order. Empty when `source` is `"apply_all"` / `"apply_all_pool"`
    /// (no per-migration names — bootstrap walks the model inventory).
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

/// Remove a previously-connected `pre_migrate` receiver.
pub fn disconnect_pre_migrate(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::PreMigrate, id)
}

/// Fire `pre_migrate` for `ctx`. Called by the framework's migrate
/// paths; available publicly so tests / custom runners can dispatch
/// the signal too.
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

/// Remove a previously-connected `post_migrate` receiver.
pub fn disconnect_post_migrate(id: ReceiverId) -> bool {
    remove_receiver(SignalKind::PostMigrate, id)
}

/// Fire `post_migrate` for `ctx`.
pub async fn send_post_migrate(ctx: PostMigrateContext) {
    let receivers: Vec<PostReceiver> = snapshot(SignalKind::PostMigrate);
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Maintenance

/// Remove **all** migrate-signal receivers. Useful in tests to reset
/// registry state between cases.
pub fn clear_all() {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Total receivers currently registered across both migrate signals.
/// Useful in tests to assert connection state.
pub fn receiver_count() -> usize {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    [SignalKind::PreMigrate, SignalKind::PostMigrate]
        .iter()
        .map(|kind| reg.get(kind).map_or(0, Vec::len))
        .sum()
}
