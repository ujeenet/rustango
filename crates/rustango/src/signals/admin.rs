//! Django-shape admin lifecycle signals — analogs to
//! `ModelAdmin.save_model()` / `ModelAdmin.delete_model()` hooks.
//! Django-parity #365.
//!
//! `post_save` / `pre_save` (declared in [`crate::signals`]) fire for
//! every ORM write across the codebase. These admin-scoped signals
//! only fire from the bundled admin's create / update / delete
//! handlers, giving operators a seam for admin-only side effects
//! (audit attribution, owner-stamping, notifications) without
//! sprinkling `if request.is_admin()` everywhere.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::signals::admin::{
//!     connect_admin_post_save, connect_admin_post_delete,
//!     AdminSaveContext, AdminDeleteContext,
//! };
//!
//! connect_admin_post_save(|ctx: AdminSaveContext| Box::pin(async move {
//!     tracing::info!(table = ctx.table, pk = %ctx.pk, change = ctx.change, "admin save");
//! }));
//! connect_admin_post_delete(|ctx: AdminDeleteContext| Box::pin(async move {
//!     tracing::info!(table = ctx.table, pk = %ctx.pk, "admin delete");
//! }));
//! ```
//!
//! ## Semantics
//!
//! - Receivers run **sequentially** in registration order.
//! - Pre-save fires before the DB write attempt; post-save only
//!   after a successful write. Same for delete.
//! - A panicking receiver aborts the chain and propagates.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

pub type ReceiverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverId(u64);

// ---------------------------------------------------------------- Context types

/// Payload delivered to `admin_pre_save` / `admin_post_save` receivers.
#[derive(Debug, Clone)]
pub struct AdminSaveContext {
    /// SQL table name of the model being saved.
    pub table: &'static str,
    /// String form of the row's primary key. Empty on `admin_pre_save`
    /// for create when the PK is server-assigned (`Auto<T>`) — the
    /// `admin_post_save` context carries the resolved PK.
    pub pk: String,
    /// `true` when this save is an update (edit form), `false` for a
    /// create (new form). Mirrors Django's `change` argument.
    pub change: bool,
}

/// Payload delivered to `admin_pre_delete` / `admin_post_delete`
/// receivers.
#[derive(Debug, Clone)]
pub struct AdminDeleteContext {
    pub table: &'static str,
    pub pk: String,
}

// ---------------------------------------------------------------- Storage

type SaveReceiver = Arc<dyn Fn(AdminSaveContext) -> ReceiverFuture + Send + Sync + 'static>;
type DeleteReceiver = Arc<dyn Fn(AdminDeleteContext) -> ReceiverFuture + Send + Sync + 'static>;

#[allow(clippy::type_complexity)]
struct Registry {
    pre_save: RwLock<HashMap<ReceiverId, SaveReceiver>>,
    post_save: RwLock<HashMap<ReceiverId, SaveReceiver>>,
    pre_delete: RwLock<HashMap<ReceiverId, DeleteReceiver>>,
    post_delete: RwLock<HashMap<ReceiverId, DeleteReceiver>>,
}

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry {
        pre_save: RwLock::new(HashMap::new()),
        post_save: RwLock::new(HashMap::new()),
        pre_delete: RwLock::new(HashMap::new()),
        post_delete: RwLock::new(HashMap::new()),
    })
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
fn next_id() -> ReceiverId {
    ReceiverId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------- pre_save

pub fn connect_admin_pre_save<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminSaveContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = next_id();
    let wrapped: SaveReceiver = Arc::new(move |ctx| -> ReceiverFuture { Box::pin(receiver(ctx)) });
    registry().pre_save.write().unwrap().insert(id, wrapped);
    id
}

pub fn disconnect_admin_pre_save(id: ReceiverId) {
    registry().pre_save.write().unwrap().remove(&id);
}

pub async fn send_admin_pre_save(ctx: AdminSaveContext) {
    let receivers: Vec<SaveReceiver> = {
        let g = registry().pre_save.read().unwrap();
        g.values().cloned().collect()
    };
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- post_save

pub fn connect_admin_post_save<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminSaveContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = next_id();
    let wrapped: SaveReceiver = Arc::new(move |ctx| -> ReceiverFuture { Box::pin(receiver(ctx)) });
    registry().post_save.write().unwrap().insert(id, wrapped);
    id
}

pub fn disconnect_admin_post_save(id: ReceiverId) {
    registry().post_save.write().unwrap().remove(&id);
}

pub async fn send_admin_post_save(ctx: AdminSaveContext) {
    let receivers: Vec<SaveReceiver> = {
        let g = registry().post_save.read().unwrap();
        g.values().cloned().collect()
    };
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- pre_delete

pub fn connect_admin_pre_delete<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminDeleteContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = next_id();
    let wrapped: DeleteReceiver =
        Arc::new(move |ctx| -> ReceiverFuture { Box::pin(receiver(ctx)) });
    registry().pre_delete.write().unwrap().insert(id, wrapped);
    id
}

pub fn disconnect_admin_pre_delete(id: ReceiverId) {
    registry().pre_delete.write().unwrap().remove(&id);
}

pub async fn send_admin_pre_delete(ctx: AdminDeleteContext) {
    let receivers: Vec<DeleteReceiver> = {
        let g = registry().pre_delete.read().unwrap();
        g.values().cloned().collect()
    };
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- post_delete

pub fn connect_admin_post_delete<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminDeleteContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = next_id();
    let wrapped: DeleteReceiver =
        Arc::new(move |ctx| -> ReceiverFuture { Box::pin(receiver(ctx)) });
    registry().post_delete.write().unwrap().insert(id, wrapped);
    id
}

pub fn disconnect_admin_post_delete(id: ReceiverId) {
    registry().post_delete.write().unwrap().remove(&id);
}

pub async fn send_admin_post_delete(ctx: AdminDeleteContext) {
    let receivers: Vec<DeleteReceiver> = {
        let g = registry().post_delete.read().unwrap();
        g.values().cloned().collect()
    };
    for r in receivers {
        r(ctx.clone()).await;
    }
}

// ---------------------------------------------------------------- Introspection

/// Number of registered receivers across all four admin signals.
/// Useful for tests that need to wait for or assert connection state.
pub fn receiver_count() -> usize {
    let r = registry();
    r.pre_save.read().unwrap().len()
        + r.post_save.read().unwrap().len()
        + r.pre_delete.read().unwrap().len()
        + r.post_delete.read().unwrap().len()
}

/// Drop every registered receiver. Tests use this to isolate state
/// between cases.
pub fn clear_all() {
    let r = registry();
    r.pre_save.write().unwrap().clear();
    r.post_save.write().unwrap().clear();
    r.pre_delete.write().unwrap().clear();
    r.post_delete.write().unwrap().clear();
}

// `Any` import keeps the file aligned with sibling signal modules in
// case a future context grows a typed-extras bag.
#[allow(dead_code)]
fn _ensure_any_imported(_: &dyn Any) {}
