//! Multi-channel notifications — fan one notification out to mail / database /
//! log / broadcast channels.
//!
//! Laravel's `Illuminate\Notifications` is the inspiration. The shape:
//!
//! 1. You define a notification struct (e.g. `WelcomeEmail`).
//! 2. You impl `Notification<User>` for it, returning a [`NotificationDispatch`]
//!    populated for each channel you want to use.
//! 3. You call `notify(&user, &notification, &ctx).await?` once. The dispatcher
//!    sends to mail / database / log / broadcast based on which fields the
//!    `NotificationDispatch` set.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::notifications::{
//!     notify, Notifiable, Notification, NotificationContext, NotificationDispatch,
//! };
//! use rustango::email::Email;
//! use serde_json::json;
//!
//! pub struct WelcomeEmail { pub display_name: String }
//!
//! impl Notification<User> for WelcomeEmail {
//!     fn build(&self, user: &User) -> NotificationDispatch {
//!         NotificationDispatch {
//!             email: Some(
//!                 Email::new()
//!                     .to(&user.email)
//!                     .from("noreply@app.example.com")
//!                     .subject("Welcome!")
//!                     .body(&format!("Hi {}, thanks for signing up.", self.display_name)),
//!             ),
//!             database: Some(json!({
//!                 "type": "user.welcome",
//!                 "display_name": self.display_name,
//!             })),
//!             log: Some(format!("welcomed user {}", user.username)),
//!             broadcast: None,
//!         }
//!     }
//! }
//!
//! impl Notifiable for User {
//!     fn notification_id(&self) -> Option<i64> { Some(self.id) }
//! }
//!
//! // At call site:
//! let ctx = NotificationContext::new()
//!     .with_mailer(mailer)
//!     .with_database(pool.clone(), "user_notifications");
//! let result = notify(&user, &WelcomeEmail { display_name: "Alice".into() }, &ctx).await?;
//! println!("delivered to {} channels", result.delivered_count());
//! ```
//!
//! ## Channels
//!
//! | Field on dispatch | Channel | Backend used |
//! |---|---|---|
//! | `email: Some(Email)` | mail | `ctx.mailer()` |
//! | `database: Some(Value)` | database | INSERT into `ctx.database_table()` |
//! | `log: Some(String)` | log | `tracing::info!` |
//! | `broadcast: Some(Value)` | broadcast | `ctx.broadcast()` callback |
//!
//! Each channel is independent — failing to send via mail does NOT abort
//! database / log delivery. The returned [`NotificationResult`] reports
//! per-channel outcomes.

use std::sync::Arc;

use crate::email::{BoxedMailer, Email};

#[cfg(feature = "cache")]
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("mail channel failed: {0}")]
    Mail(String),
    #[error("database channel failed: {0}")]
    Database(String),
    #[error("broadcast channel failed: {0}")]
    Broadcast(String),
}

// ------------------------------------------------------------------ Notifiable

/// Marker for things that can receive notifications. Implement on your
/// `User`, `Operator`, etc. Most apps only need `notification_id` —
/// the email goes into the `Email` builder by the `Notification` itself.
pub trait Notifiable {
    /// Stable identifier stored alongside database notifications. Returns
    /// `None` to skip database channel for this notifiable.
    fn notification_id(&self) -> Option<i64> {
        None
    }

    /// Optional preferred locale — receivers can use this to choose a
    /// translation. Default `None` (English).
    fn notification_locale(&self) -> Option<String> {
        None
    }
}

// ------------------------------------------------------------------ Notification trait + dispatch

/// One notification, possibly delivered across several channels.
///
/// Each `to_*` field is optional; populated fields are sent. An empty
/// dispatch is a no-op (legal — useful when notifications are conditional
/// on per-recipient settings).
#[derive(Debug, Clone, Default)]
pub struct NotificationDispatch {
    pub email: Option<Email>,
    pub database: Option<serde_json::Value>,
    pub log: Option<String>,
    pub broadcast: Option<serde_json::Value>,
}

impl NotificationDispatch {
    /// Empty dispatch — no channels.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Convenience: an email-only dispatch.
    #[must_use]
    pub fn email_only(email: Email) -> Self {
        Self { email: Some(email), ..Self::default() }
    }
}

/// A notification that can be sent to one or more receivers of type `N`.
pub trait Notification<N: Notifiable> {
    /// Build the per-channel payloads for `recipient`. Return
    /// [`NotificationDispatch::none()`] to skip this recipient entirely.
    fn build(&self, recipient: &N) -> NotificationDispatch;
}

// ------------------------------------------------------------------ Context

/// Broadcast callback — invoked once per `notify()` call when the dispatch
/// has a `broadcast` payload. Wire a WebSocket / SSE / pub-sub here.
pub type BroadcastFn = Arc<
    dyn Fn(serde_json::Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// Per-app dispatch context — holds the channel backends.
///
/// Build once at startup and pass by reference to every `notify()` call.
/// Channels you don't configure get silently skipped.
#[derive(Default, Clone)]
pub struct NotificationContext {
    mailer: Option<BoxedMailer>,
    database_pool: Option<crate::sql::sqlx::PgPool>,
    database_table: Option<String>,
    broadcast: Option<BroadcastFn>,
}

impl NotificationContext {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_mailer(mut self, mailer: BoxedMailer) -> Self {
        self.mailer = Some(mailer);
        self
    }

    /// Configure the database channel — INSERTs into `table` with columns:
    /// `(notifiable_id BIGINT, type TEXT, data JSONB, created_at TIMESTAMPTZ DEFAULT NOW())`.
    /// Create the table separately via your migrations.
    #[must_use]
    pub fn with_database(
        mut self,
        pool: crate::sql::sqlx::PgPool,
        table: impl Into<String>,
    ) -> Self {
        self.database_pool = Some(pool);
        self.database_table = Some(table.into());
        self
    }

    /// Configure the broadcast channel.
    #[must_use]
    pub fn with_broadcast<F, Fut>(mut self, callback: F) -> Self
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        self.broadcast = Some(Arc::new(move |v| Box::pin(callback(v))));
        self
    }
}

// ------------------------------------------------------------------ Result

/// Per-channel outcome of one [`notify`] call.
#[derive(Debug, Clone, Default)]
pub struct NotificationResult {
    pub mail: ChannelOutcome,
    pub database: ChannelOutcome,
    pub log: ChannelOutcome,
    pub broadcast: ChannelOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChannelOutcome {
    /// Channel was not used (no payload + skip).
    #[default]
    Skipped,
    /// Channel delivered the payload.
    Delivered,
    /// Channel failed; the message contains the error.
    Failed(String),
}

impl NotificationResult {
    /// Number of channels that successfully delivered.
    #[must_use]
    pub fn delivered_count(&self) -> usize {
        [&self.mail, &self.database, &self.log, &self.broadcast]
            .iter()
            .filter(|o| matches!(o, ChannelOutcome::Delivered))
            .count()
    }

    /// `true` when at least one channel delivered.
    #[must_use]
    pub fn any_delivered(&self) -> bool {
        self.delivered_count() > 0
    }

    /// `true` when no channel failed (Skipped + Delivered are both ok).
    #[must_use]
    pub fn no_failures(&self) -> bool {
        ![&self.mail, &self.database, &self.log, &self.broadcast]
            .iter()
            .any(|o| matches!(o, ChannelOutcome::Failed(_)))
    }
}

// ------------------------------------------------------------------ notify() — single recipient

/// Send `notification` to `recipient` across every configured channel.
///
/// Per-channel failures are recorded in the returned [`NotificationResult`]
/// but do not abort other channels. Returns `Err` only when serialization /
/// schema-side errors prevent a channel from running at all (rare).
pub async fn notify<N: Notifiable, T: Notification<N>>(
    recipient: &N,
    notification: &T,
    ctx: &NotificationContext,
) -> NotificationResult {
    let dispatch = notification.build(recipient);
    let mut result = NotificationResult::default();

    // Mail
    if let Some(email) = &dispatch.email {
        result.mail = match &ctx.mailer {
            Some(m) => match m.send(email).await {
                Ok(()) => ChannelOutcome::Delivered,
                Err(e) => ChannelOutcome::Failed(e.to_string()),
            },
            None => ChannelOutcome::Failed("no mailer configured".into()),
        };
    }

    // Database
    if let Some(payload) = &dispatch.database {
        result.database = match (&ctx.database_pool, &ctx.database_table, recipient.notification_id()) {
            (Some(pool), Some(table), Some(id)) => {
                let kind = payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("notification")
                    .to_owned();
                match insert_database_notification(pool, table, id, &kind, payload).await {
                    Ok(()) => ChannelOutcome::Delivered,
                    Err(e) => ChannelOutcome::Failed(e),
                }
            }
            (None, _, _) | (_, None, _) => {
                ChannelOutcome::Failed("database channel not configured".into())
            }
            (_, _, None) => ChannelOutcome::Skipped, // recipient opted out via notification_id
        };
    }

    // Log
    if let Some(line) = &dispatch.log {
        tracing::info!(notification = %line, "notifications");
        result.log = ChannelOutcome::Delivered;
    }

    // Broadcast
    if let Some(payload) = &dispatch.broadcast {
        result.broadcast = match &ctx.broadcast {
            Some(callback) => match callback(payload.clone()).await {
                Ok(()) => ChannelOutcome::Delivered,
                Err(e) => ChannelOutcome::Failed(e),
            },
            None => ChannelOutcome::Failed("broadcast channel not configured".into()),
        };
    }

    result
}

/// Send the same notification to many recipients sequentially. Convenient
/// when the notification doesn't vary per-user. Returns one result per recipient.
pub async fn notify_many<N: Notifiable, T: Notification<N>>(
    recipients: &[&N],
    notification: &T,
    ctx: &NotificationContext,
) -> Vec<NotificationResult> {
    let mut out = Vec::with_capacity(recipients.len());
    for r in recipients {
        out.push(notify(*r, notification, ctx).await);
    }
    out
}

async fn insert_database_notification(
    pool: &crate::sql::sqlx::PgPool,
    table: &str,
    notifiable_id: i64,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), String> {
    validate_table_name(table)?;
    let sql = format!(
        r#"INSERT INTO "{table}" ("notifiable_id", "type", "data") VALUES ($1, $2, $3)"#,
    );
    crate::sql::sqlx::query(&sql)
        .bind(notifiable_id)
        .bind(kind)
        .bind(payload)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())
        .map(|_| ())
}

/// Reject table names with characters that could break the quoted form.
fn validate_table_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("table name is empty".into());
    }
    let bad = ['"', '\0', '\n', '\r', '\\', ';', ' '];
    if name.chars().any(|c| bad.contains(&c) || c.is_control()) {
        return Err(format!("table name `{name}` contains forbidden characters"));
    }
    Ok(())
}

// ------------------------------------------------------------------ Throttling helper (cache-feature-gated)

/// Track whether a notification has been sent recently — useful for
/// rate-limiting "X new comments" emails or similar burst-prone events.
///
/// Returns `true` when the notification is allowed (cache key was not present
/// and is now set to expire after `ttl`); `false` when it should be skipped
/// (already sent within `ttl`).
#[cfg(feature = "cache")]
pub async fn should_send_throttled(
    cache: &dyn crate::cache::Cache,
    key: &str,
    ttl: Duration,
) -> bool {
    if cache.exists(key).await.unwrap_or(false) {
        return false;
    }
    let _ = cache.set(key, "1", Some(ttl)).await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::{Email, InMemoryMailer, Mailer};
    use std::sync::Mutex;

    struct TestUser { id: i64, email: String }
    impl Notifiable for TestUser {
        fn notification_id(&self) -> Option<i64> { Some(self.id) }
    }

    struct WelcomeEmail;
    impl Notification<TestUser> for WelcomeEmail {
        fn build(&self, user: &TestUser) -> NotificationDispatch {
            NotificationDispatch {
                email: Some(
                    Email::new()
                        .to(&user.email)
                        .from("noreply@app")
                        .subject("Welcome")
                        .body("Hi"),
                ),
                log: Some(format!("welcomed user {}", user.id)),
                ..NotificationDispatch::default()
            }
        }
    }

    #[tokio::test]
    async fn dispatch_sends_to_configured_channels() {
        let mailer = Arc::new(InMemoryMailer::new());
        let mailer_clone = mailer.clone();
        let ctx = NotificationContext::new().with_mailer(mailer_clone as _);

        let u = TestUser { id: 1, email: "a@x.com".into() };
        let r = notify(&u, &WelcomeEmail, &ctx).await;

        assert_eq!(r.mail, ChannelOutcome::Delivered);
        assert_eq!(r.log, ChannelOutcome::Delivered);
        assert_eq!(r.database, ChannelOutcome::Skipped);    // no payload
        assert_eq!(r.broadcast, ChannelOutcome::Skipped);
        assert_eq!(mailer.count(), 1);
    }

    #[tokio::test]
    async fn missing_mailer_records_failure_without_aborting_other_channels() {
        let ctx = NotificationContext::new(); // no mailer
        let u = TestUser { id: 1, email: "a@x.com".into() };
        let r = notify(&u, &WelcomeEmail, &ctx).await;

        assert!(matches!(r.mail, ChannelOutcome::Failed(_)));
        assert_eq!(r.log, ChannelOutcome::Delivered, "log channel should still fire");
        assert!(!r.no_failures());
        assert!(r.any_delivered());
    }

    #[tokio::test]
    async fn empty_dispatch_skips_everything() {
        struct NoOp;
        impl Notification<TestUser> for NoOp {
            fn build(&self, _user: &TestUser) -> NotificationDispatch {
                NotificationDispatch::none()
            }
        }
        let ctx = NotificationContext::new();
        let u = TestUser { id: 1, email: "a@x.com".into() };
        let r = notify(&u, &NoOp, &ctx).await;
        assert_eq!(r.mail, ChannelOutcome::Skipped);
        assert_eq!(r.log, ChannelOutcome::Skipped);
        assert_eq!(r.database, ChannelOutcome::Skipped);
        assert_eq!(r.broadcast, ChannelOutcome::Skipped);
        assert!(r.no_failures());
        assert_eq!(r.delivered_count(), 0);
    }

    #[tokio::test]
    async fn broadcast_callback_fires() {
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let ctx = NotificationContext::new().with_broadcast(move |payload| {
            let cap = cap.clone();
            async move {
                cap.lock().unwrap().push(payload);
                Ok(())
            }
        });

        struct WithBroadcast;
        impl Notification<TestUser> for WithBroadcast {
            fn build(&self, _user: &TestUser) -> NotificationDispatch {
                NotificationDispatch {
                    broadcast: Some(serde_json::json!({"event": "ping"})),
                    ..NotificationDispatch::default()
                }
            }
        }

        let u = TestUser { id: 1, email: "a@x.com".into() };
        let r = notify(&u, &WithBroadcast, &ctx).await;
        assert_eq!(r.broadcast, ChannelOutcome::Delivered);
        assert_eq!(captured.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn notify_many_returns_one_result_per_recipient() {
        let ctx = NotificationContext::new();
        let users = vec![
            TestUser { id: 1, email: "a@x".into() },
            TestUser { id: 2, email: "b@x".into() },
            TestUser { id: 3, email: "c@x".into() },
        ];
        let refs: Vec<&TestUser> = users.iter().collect();
        let results = notify_many(&refs, &WelcomeEmail, &ctx).await;
        assert_eq!(results.len(), 3);
        for r in &results {
            assert_eq!(r.log, ChannelOutcome::Delivered);
        }
    }

    #[tokio::test]
    async fn delivered_count_matches_actual_deliveries() {
        let mailer: Arc<dyn Mailer> = Arc::new(InMemoryMailer::new());
        let ctx = NotificationContext::new().with_mailer(mailer);
        let u = TestUser { id: 1, email: "a@x".into() };
        let r = notify(&u, &WelcomeEmail, &ctx).await;
        assert_eq!(r.delivered_count(), 2); // mail + log
    }

    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn throttle_helper_allows_first_send_blocks_repeat() {
        use crate::cache::InMemoryCache;
        let cache = InMemoryCache::new();
        assert!(should_send_throttled(&cache, "k", Duration::from_secs(60)).await);
        assert!(!should_send_throttled(&cache, "k", Duration::from_secs(60)).await);
    }
}
