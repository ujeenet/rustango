//! Where a provisioning run lives while it happens, and after.
//!
//! Slices [#1318] and [#1320] produce a stream of events. Holding them
//! only in memory fails two ways that matter:
//!
//! **Replay.** [`crate::sse::EventBus`] is a `tokio::sync::broadcast`
//! wrapper, so a subscriber sees only what is sent *after* it connects.
//! Provisioning starts on a POST and the browser connects on the *next*
//! request — the opening steps are gone before anyone is listening, and
//! a mid-run reload loses the rest.
//!
//! **Two pods.** Behind a load balancer the operator watching the
//! stream may not be on the pod doing the work. In-process state means
//! they watch an empty page while provisioning runs fine elsewhere.
//! This is the same problem the resolver solved with a registry
//! fingerprint rather than a shared cache: put it in the database both
//! pods already have.
//!
//! [#1318]: https://github.com/ujeenet/rustango/issues/1318
//! [#1320]: https://github.com/ujeenet/rustango/issues/1320
//!
//! ## Credentials are never stored
//!
//! A run records the URL it was asked to provision, and a connection
//! URL has a password in it. [`crate::sql::connect_diagnosis::redact`]
//! is applied on the way in — not on the way out, so there is no
//! reading path that can forget it. The same goes for event messages,
//! which carry connection diagnoses.

use serde::Serialize;

use crate::core::Column as _;
use crate::sql::{Auto, FetcherPool, Pool, UpdaterPool};

use super::error::TenancyError;

/// Where a provisioning run has got to.
///
/// Stored as a short string rather than an enum column so a state can
/// be added without a migration, and so a human reading the table with
/// `psql` can see what it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Accepted, not started. The state a webhook returns on.
    Pending,
    Running,
    Succeeded,
    /// Stopped at some step. `ProvisioningRun::error` says which.
    Failed,
}

impl RunState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    /// Parse a stored value. Unknown strings read as `Failed` rather
    /// than panicking: a row written by a newer build should not take
    /// down an older one, and "something went wrong" is the safe
    /// reading of a state this binary does not know.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            _ => Self::Failed,
        }
    }

    /// Nothing more will happen to this run.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// One attempt to stand up a tenant.
#[derive(crate::Model, Debug, Clone, Serialize)]
#[rustango(
    table = "rustango_provisioning_runs",
    scope = "registry",
    display = "slug",
    admin(
        list_display = "slug, state, started_at, finished_at",
        ordering = "-started_at",
    )
)]
pub struct ProvisioningRun {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,

    /// The tenant this run is for. Not a foreign key: the row is
    /// written *before* the `Org` exists, and must survive the tenant
    /// later being purged — a run is a record of what happened, and an
    /// `ON DELETE CASCADE` would erase exactly the history someone
    /// comes looking for.
    #[rustango(max_length = 100, index)]
    pub slug: String,

    /// Set once the `Org` row lands. `None` while a run is still before
    /// that step, or if it failed before reaching it.
    pub org_id: Option<i64>,

    /// `pending` / `running` / `succeeded` / `failed`. See [`RunState`].
    #[rustango(max_length = 16, index)]
    pub state: String,

    #[rustango(max_length = 16)]
    pub storage_mode: String,

    #[rustango(max_length = 16)]
    pub backend_kind: String,

    /// **Redacted.** The password is replaced before this is stored;
    /// see the module docs.
    #[rustango(max_length = 500)]
    pub database_url: Option<String>,

    /// Who asked for this run — an operator username, a webhook's
    /// caller id, or `cli`. Free text on purpose: the sources are not
    /// one namespace.
    #[rustango(max_length = 150)]
    pub requested_by: Option<String>,

    /// The caller-supplied key that makes a retried request return this
    /// run instead of creating a second tenant. Unique so the database,
    /// not the handler, is what wins a race between two simultaneous
    /// deliveries of one event.
    #[rustango(max_length = 200, unique)]
    pub idempotency_key: Option<String>,

    /// Why it failed, if it did. Already rendered for display.
    pub error: Option<String>,

    #[rustango(auto_now_add)]
    pub started_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,

    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// One thing that happened during a run.
///
/// The stream a console replays. Deliberately append-only: an event is
/// a fact about the past, and the value of the log is that it is what
/// actually happened rather than a summary someone kept up to date.
#[derive(crate::Model, Debug, Clone, Serialize)]
#[rustango(
    table = "rustango_provisioning_events",
    scope = "registry",
    display = "message",
    admin(list_display = "run_id, seq, step, status", ordering = "run_id, seq")
)]
pub struct ProvisioningEvent {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,

    #[rustango(fk = "rustango_provisioning_runs", on = "id", index)]
    pub run_id: i64,

    /// Monotonic within a run, from 1. **This is what `Last-Event-ID`
    /// resumes from**, so it has to be dense and ordered — an
    /// autoincrement `id` would be globally ordered but not per-run,
    /// and a reconnecting client would have nothing to count from.
    pub seq: i64,

    /// Which provisioning step, or `migration` for a forwarded
    /// migration event.
    #[rustango(max_length = 32)]
    pub step: String,

    /// `started` / `ok` / `skipped` / `failed` / `info`.
    #[rustango(max_length = 16)]
    pub status: String,

    /// Human-readable detail. Empty for a bare transition.
    pub message: String,

    #[rustango(auto_now_add)]
    pub at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

/// Open a run, before any work starts.
///
/// `database_url` is redacted here rather than by the caller: this is
/// the only way into the table, so redacting on the way in means there
/// is no path that can forget.
///
/// # Errors
/// A registry write failure — or a unique-constraint violation on
/// `idempotency_key`, which is the intended way two simultaneous
/// deliveries of one event resolve to one run.
pub async fn open_run(
    registry: &Pool,
    slug: &str,
    storage_mode: &str,
    backend_kind: &str,
    database_url: Option<&str>,
    requested_by: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<ProvisioningRun, TenancyError> {
    let mut run = ProvisioningRun {
        id: Auto::default(),
        slug: slug.to_owned(),
        org_id: None,
        state: RunState::Running.as_str().to_owned(),
        storage_mode: storage_mode.to_owned(),
        backend_kind: backend_kind.to_owned(),
        database_url: database_url.map(crate::sql::connect_diagnosis::redact),
        requested_by: requested_by.map(ToOwned::to_owned),
        idempotency_key: idempotency_key.map(ToOwned::to_owned),
        error: None,
        started_at: Auto::default(),
        finished_at: None,
    };
    run.insert_pool(registry).await?;
    Ok(run)
}

/// Append an event to a run.
///
/// `seq` is supplied by the caller rather than computed here: the
/// caller is emitting a known sequence and a `SELECT MAX(seq)` per
/// event would be a round-trip per line of a progress stream, plus a
/// race between two writers.
///
/// # Errors
/// A registry write failure.
pub async fn append_event(
    registry: &Pool,
    run_id: i64,
    seq: i64,
    step: &str,
    status: &str,
    message: &str,
) -> Result<(), TenancyError> {
    let mut event = ProvisioningEvent {
        id: Auto::default(),
        run_id,
        seq,
        step: step.to_owned(),
        status: status.to_owned(),
        message: message.to_owned(),
        at: Auto::default(),
    };
    event.insert_pool(registry).await?;
    Ok(())
}

/// Record that the `Org` row now exists, so a run that fails later
/// still says which tenant it half-made.
///
/// # Errors
/// A registry write failure.
pub async fn attach_org(registry: &Pool, run_id: i64, org_id: i64) -> Result<(), TenancyError> {
    ProvisioningRun::objects()
        .where_(ProvisioningRun::id.eq(run_id))
        .update()
        .set("org_id", org_id)
        .execute_pool(registry)
        .await?;
    Ok(())
}

/// Close a run.
///
/// # Errors
/// A registry write failure.
pub async fn finish_run(
    registry: &Pool,
    run_id: i64,
    state: RunState,
    error: Option<&str>,
) -> Result<(), TenancyError> {
    ProvisioningRun::objects()
        .where_(ProvisioningRun::id.eq(run_id))
        .update()
        .set("state", state.as_str())
        .set("error", error)
        .set("finished_at", chrono::Utc::now())
        .execute_pool(registry)
        .await?;
    Ok(())
}

/// Read a run by id.
///
/// # Errors
/// A registry read failure.
pub async fn run_by_id(
    registry: &Pool,
    run_id: i64,
) -> Result<Option<ProvisioningRun>, TenancyError> {
    Ok(ProvisioningRun::objects()
        .where_(ProvisioningRun::id.eq(run_id))
        .fetch(registry)
        .await?
        .into_iter()
        .next())
}

/// Find the run a caller's idempotency key already opened.
///
/// # Errors
/// A registry read failure.
pub async fn run_by_idempotency_key(
    registry: &Pool,
    key: &str,
) -> Result<Option<ProvisioningRun>, TenancyError> {
    Ok(ProvisioningRun::objects()
        .where_(ProvisioningRun::idempotency_key.eq(Some(key.to_owned())))
        .fetch(registry)
        .await?
        .into_iter()
        .next())
}

/// Replay a run's events, in order, from `after_seq` exclusive.
///
/// `after_seq` of 0 is the whole log — what a fresh subscriber wants.
/// A reconnecting one passes the last `seq` it saw (its
/// `Last-Event-ID`) and gets only what it missed.
///
/// # Errors
/// A registry read failure.
pub async fn events_since(
    registry: &Pool,
    run_id: i64,
    after_seq: i64,
) -> Result<Vec<ProvisioningEvent>, TenancyError> {
    Ok(ProvisioningEvent::objects()
        .where_(ProvisioningEvent::run_id.eq(run_id))
        .where_(ProvisioningEvent::seq.gt(after_seq))
        // Ascending — the bool is `desc`. A replay handed back newest
        // first is not a replay.
        .order_by(&[("seq", false)])
        .fetch(registry)
        .await?)
}

/// The most recent runs, newest first.
///
/// A run is reachable by id and nothing enumerated them, so a console
/// could show a run only while its redirect was still in the address
/// bar — navigate away and the record survived in the table but not in
/// anybody's reach. That is the opposite of why the table is persisted.
///
/// `limit` is a page size rather than a promise to return everything:
/// the table grows with provisioning activity and a console asking for
/// "recent" wants a screenful.
///
/// # Errors
/// Driver / query failures.
pub async fn recent_runs(
    registry: &Pool,
    limit: i64,
    offset: i64,
) -> Result<Vec<ProvisioningRun>, TenancyError> {
    Ok(ProvisioningRun::objects()
        // Descending — the bool is `desc`, and "recent" means the run
        // somebody just started is the one at the top.
        .order_by(&[("id", true)])
        .limit(limit)
        .offset(offset)
        .fetch(registry)
        .await?)
}

/// Delete finished runs older than `cutoff`, and their events.
///
/// Runs are append-only and one per tenant creation, so the table grows
/// with provisioning activity and nothing removes it. Unfinished runs
/// are never pruned regardless of age — one stuck in `running` is
/// evidence of a pod that died mid-provision, which is exactly what
/// someone will come looking for.
///
/// Returns how many runs were removed.
///
/// # Errors
/// A registry write failure.
pub async fn prune_runs(
    registry: &Pool,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<usize, TenancyError> {
    let stale: Vec<ProvisioningRun> = ProvisioningRun::objects()
        .where_(ProvisioningRun::finished_at.lt(Some(cutoff)))
        .fetch(registry)
        .await?;

    let mut removed = 0;
    for run in stale {
        let Some(id) = run.id.get().copied() else {
            continue;
        };
        // Children first: the FK points this way, and a database with
        // it enforced would refuse the parent delete otherwise.
        //
        // Row at a time because the ORM has no queryset-level delete
        // through a `Pool`. Acceptable here — this is a maintenance
        // verb over a couple of dozen events per run, not a request
        // path — but it is why `prune_runs` is not something to call
        // in a loop.
        let events: Vec<ProvisioningEvent> = ProvisioningEvent::objects()
            .where_(ProvisioningEvent::run_id.eq(id))
            .fetch(registry)
            .await?;
        for event in events {
            event.delete_pool(registry).await?;
        }
        run.delete_pool(registry).await?;
        removed += 1;
    }
    Ok(removed)
}
