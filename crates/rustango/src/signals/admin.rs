//! Save and delete signals that fire only for writes made through
//! the bundled admin.
//!
//! The signals in [`crate::signals`] cover every ORM write anywhere.
//! These ones come only from the bundled admin's create, update, delete
//! and bulk-action handlers (one signal per row), so admin-only side effects — audit attribution,
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
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

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

type Receiver<C> = Arc<dyn Fn(C) -> ReceiverFuture + Send + Sync + 'static>;

/// One signal's receivers, kept in registration order (#1928).
struct Channel<C>(RwLock<Vec<(ReceiverId, Receiver<C>)>>);

impl<C: Clone + Send + 'static> Channel<C> {
    const fn new() -> Self {
        Self(RwLock::new(Vec::new()))
    }

    fn connect<F, Fut>(&self, receiver: F) -> ReceiverId
    where
        F: Fn(C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = next_id();
        let wrapped: Receiver<C> =
            Arc::new(move |ctx| -> ReceiverFuture { Box::pin(receiver(ctx)) });
        self.0.write().unwrap().push((id, wrapped));
        id
    }

    fn disconnect(&self, id: ReceiverId) {
        self.0.write().unwrap().retain(|(rid, _)| *rid != id);
    }

    async fn send(&self, ctx: C) {
        let receivers: Vec<Receiver<C>> = self
            .0
            .read()
            .unwrap()
            .iter()
            .map(|(_, r)| r.clone())
            .collect();
        for r in receivers {
            r(ctx.clone()).await;
        }
    }

    fn len(&self) -> usize {
        self.0.read().unwrap().len()
    }

    fn clear(&self) {
        self.0.write().unwrap().clear();
    }
}

static PRE_SAVE: Channel<AdminSaveContext> = Channel::new();
static POST_SAVE: Channel<AdminSaveContext> = Channel::new();
static PRE_DELETE: Channel<AdminDeleteContext> = Channel::new();
static POST_DELETE: Channel<AdminDeleteContext> = Channel::new();

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
    PRE_SAVE.connect(receiver)
}

pub fn disconnect_admin_pre_save(id: ReceiverId) {
    PRE_SAVE.disconnect(id);
}

pub async fn send_admin_pre_save(ctx: AdminSaveContext) {
    PRE_SAVE.send(ctx).await;
}

// ---------------------------------------------------------------- post_save

pub fn connect_admin_post_save<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminSaveContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    POST_SAVE.connect(receiver)
}

pub fn disconnect_admin_post_save(id: ReceiverId) {
    POST_SAVE.disconnect(id);
}

pub async fn send_admin_post_save(ctx: AdminSaveContext) {
    POST_SAVE.send(ctx).await;
}

// ---------------------------------------------------------------- pre_delete

pub fn connect_admin_pre_delete<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminDeleteContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    PRE_DELETE.connect(receiver)
}

pub fn disconnect_admin_pre_delete(id: ReceiverId) {
    PRE_DELETE.disconnect(id);
}

pub async fn send_admin_pre_delete(ctx: AdminDeleteContext) {
    PRE_DELETE.send(ctx).await;
}

// ---------------------------------------------------------------- post_delete

pub fn connect_admin_post_delete<F, Fut>(receiver: F) -> ReceiverId
where
    F: Fn(AdminDeleteContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    POST_DELETE.connect(receiver)
}

pub fn disconnect_admin_post_delete(id: ReceiverId) {
    POST_DELETE.disconnect(id);
}

pub async fn send_admin_post_delete(ctx: AdminDeleteContext) {
    POST_DELETE.send(ctx).await;
}

// ---------------------------------------------------------------- Introspection

/// How many receivers are registered across the four admin signals.
/// Mostly useful in tests.
pub fn receiver_count() -> usize {
    PRE_SAVE.len() + POST_SAVE.len() + PRE_DELETE.len() + POST_DELETE.len()
}

/// Remove every receiver. Tests use it to isolate cases.
pub fn clear_all() {
    PRE_SAVE.clear();
    POST_SAVE.clear();
    PRE_DELETE.clear();
    POST_DELETE.clear();
}

// Keeps the `Any` import, so this file matches the other signal
// modules if a context ever grows a typed extras bag.
#[allow(dead_code)]
fn _ensure_any_imported(_: &dyn Any) {}
