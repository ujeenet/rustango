//! Save and delete signals that fire only for writes made through
//! the bundled admin.
//!
//! The signals in [`crate::signals`] cover every ORM write anywhere.
//! These ones come only from the bundled admin's create, update and
//! delete handlers, so admin-only side effects — audit attribution,
//! owner stamping, notifications — need no `if request.is_admin()`
//! check.
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
//! ## Rules
//!
//! - Receivers run one at a time, in registration order.
//! - A pre-save runs before the write is attempted; a post-save only
//!   after it succeeds. Delete works the same way.
//! - A panic in a receiver stops the rest of the chain.

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

/// What an `admin_pre_save` or `admin_post_save` receiver gets.
#[derive(Debug, Clone)]
pub struct AdminSaveContext {
    /// Table name of the model being saved.
    pub table: &'static str,
    /// The primary key as a string. It is empty on `admin_pre_save`
    /// for a create with a server-assigned key; `admin_post_save`
    /// then carries the real one.
    pub pk: String,
    /// `true` for an edit, `false` for a create.
    pub change: bool,
}

/// What an `admin_pre_delete` or `admin_post_delete` receiver gets.
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

/// How many receivers are registered across the four admin signals.
/// Mostly useful in tests.
pub fn receiver_count() -> usize {
    let r = registry();
    r.pre_save.read().unwrap().len()
        + r.post_save.read().unwrap().len()
        + r.pre_delete.read().unwrap().len()
        + r.post_delete.read().unwrap().len()
}

/// Remove every receiver. Tests use it to isolate cases.
pub fn clear_all() {
    let r = registry();
    r.pre_save.write().unwrap().clear();
    r.post_save.write().unwrap().clear();
    r.pre_delete.write().unwrap().clear();
    r.post_delete.write().unwrap().clear();
}

// Keeps the `Any` import, so this file matches the other signal
// modules if a context ever grows a typed extras bag.
#[allow(dead_code)]
fn _ensure_any_imported(_: &dyn Any) {}
