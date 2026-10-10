//! Standing up a tenant: the steps, in order, with no terminal in
//! them.
//!
//! The `create-tenant` verb keeps only what is a CLI's job: turning
//! `argv` into a [`ProvisionRequest`], and printing
//! [`ProvisionEvent`]s. An HTTP handler, a webhook or a job can drive
//! the same sequence.
//!
//! ## The steps
//!
//! 1. [`Validate`][step] — slug, mode and backend agree with each
//!    other and with this build.
//! 2. [`CheckConnection`][step] — reach the tenant's database and
//!    prove this role can create tables there, **before** anything is
//!    written. See [`preflight`](crate::tenancy::preflight). Schema
//!    mode skips it; those tenants live in the registry's own
//!    database.
//! 3. [`ProvisionStorage`][step] — `CREATE SCHEMA` for schema mode,
//!    refusing one that exists. A failed `INSERT` drops it again.
//! 4. [`RegisterOrg`][step] — the `rustango_orgs` row, written
//!    **inactive**.
//! 5. [`Migrate`][step] — this tenant's schema, via
//!    [`tenant_migrate::migrate_one_tenant`].
//! 6. [`Activate`][step] — set `active`, and the tenant starts
//!    resolving.
//!
//! ## What a failed run leaves behind
//!
//! The row is written inactive and activated last, so a
//! half-provisioned tenant serves nothing: the resolver filters on
//! `active`. No tenant ever resolves to a database with no schema.
//!
//! A run that fails at migrate leaves an inactive `Org` row and
//! whatever schema was created. The row stays on purpose: an operator
//! needs to see what was half-made, and rolling back is not always
//! possible once a schema exists.
//!
//! So `active = false` now means either "suspended" or "never
//! finished". The resolver treats both the same way. To tell them
//! apart, read [`provision_store`](crate::tenancy::provision_store).
//!
//! [`ProvisionRequest`]: crate::tenancy::provision::ProvisionRequest
//! [`ProvisionEvent`]: crate::tenancy::provision::ProvisionEvent
//! [step]: crate::tenancy::provision::ProvisionStep
//! [`tenant_migrate::migrate_one_tenant`]: crate::tenancy::migrate::migrate_one_tenant

use std::path::Path;

use sqlx::Database;

use crate::core::Column as _;
use crate::migrate::file::Migration;
use crate::sql::{Auto, FetcherPool};

use super::error::TenancyError;
use super::migrate as tenant_migrate;
use super::org::{BackendKind, Org, StorageMode};
use super::pools::TenantPools;
use super::preflight;
use super::provision_store::RunText;

/// Everything needed to stand up one tenant.
///
/// The CLI's `--no-migrate` flag is
/// [`run_migrations`](Self::run_migrations) here: a negative flag reads
/// well on a command line and badly on a field.
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    /// Globally unique. Also the default schema name, display name, and
    /// subdomain label.
    pub slug: String,
    pub mode: StorageMode,
    pub backend: BackendKind,
    /// Defaults to the slug.
    pub display_name: Option<String>,
    /// Required in database-mode; meaningless in schema-mode.
    pub database_url: Option<String>,
    /// Schema-mode only; defaults to the slug.
    pub schema_name: Option<String>,
    /// Defaults to `<slug>.<RUSTANGO_APEX_DOMAIN>` when that env var is
    /// set, and to nothing when it is not.
    pub host_pattern: Option<String>,
    pub port: Option<i32>,
    pub path_prefix: Option<String>,
    /// Run the tenant's migrations once the row is in place.
    pub run_migrations: bool,
    /// How hard to check the target database before writing anything.
    /// Defaults to a full check including the write probe — see
    /// [`preflight`] for why a bare `SELECT 1` is not enough.
    pub preflight: preflight::Preflight,
}

impl ProvisionRequest {
    /// A database-mode request with everything else defaulted.
    #[must_use]
    pub fn database(slug: impl Into<String>, database_url: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            mode: StorageMode::Database,
            backend: BackendKind::default(),
            display_name: None,
            database_url: Some(database_url.into()),
            schema_name: None,
            host_pattern: None,
            port: None,
            path_prefix: None,
            run_migrations: true,
            preflight: preflight::Preflight::default(),
        }
    }
}

/// One stage of provisioning, reportable on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionStep {
    /// Slug is free; mode, backend and build agree.
    Validate,
    /// Reach the target database, and prove this role can create
    /// tables in it, before anything is written.
    CheckConnection,
    /// `CREATE SCHEMA` for schema-mode. Nothing to do in database-mode.
    ProvisionStorage,
    /// Insert the `rustango_orgs` row — **inactive**, so nothing routes
    /// to the tenant until its schema is in place.
    RegisterOrg,
    /// Apply the tenant's migrations.
    Migrate,
    /// Set `active`, after which the tenant resolves. Last on purpose,
    /// so a half-provisioned tenant never serves a request.
    Activate,
}

/// How a step went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Started,
    Ok,
    /// Nothing to do, and why. Separate from `Ok` so a console can grey
    /// the step out instead of claiming work that never happened.
    Skipped(String),
    /// The run stops here.
    Failed(String),
}

/// Something that happened while provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionEvent {
    Step {
        step: ProvisionStep,
        status: StepStatus,
    },
    /// The row landed and the tenant now resolves. Separate from
    /// `Step { RegisterOrg, Ok }` because it carries the id, which is
    /// the one thing a caller cannot compute for itself.
    Registered { org_id: i64 },
    /// A migration event from the tenant's own run, passed straight
    /// through, so a watcher sees what `manage migrate` prints.
    Migration(tenant_migrate::TenantMigrationEvent),
}

/// Receives [`ProvisionEvent`]s as a run progresses.
///
/// Implemented for any `Fn(ProvisionEvent)`, so a closure works.
/// **Must not block**: migration events reach it while the tenant's
/// migrate lock is held. See [`crate::migrate::progress`].
pub trait ProvisionObserver: Send + Sync {
    /// Handle one event. Must return promptly and must not panic.
    fn on_event(&self, event: ProvisionEvent);
}

impl<F> ProvisionObserver for F
where
    F: Fn(ProvisionEvent) + Send + Sync,
{
    fn on_event(&self, event: ProvisionEvent) {
        self(event);
    }
}

/// What the tenant's migration step did.
#[derive(Debug, Clone)]
pub enum MigrationsOutcome {
    /// The caller asked for no migrations.
    Skipped,
    /// The batch ran but reported nothing for this tenant, usually a
    /// migrations directory with nothing tenant-scoped in it.
    NotMatched,
    Applied(Vec<Migration>),
    /// The tenant's migrations failed. Not an `Err`, because the org
    /// row exists and the caller must hear about it.
    Failed(String),
}

/// The result of a completed provisioning run.
#[derive(Debug, Clone)]
pub struct ProvisionOutcome {
    pub org_id: i64,
    pub slug: String,
    pub mode: StorageMode,
    pub migrations: MigrationsOutcome,
}

/// Where a run reports: the caller's observer, and optionally the
/// durable store.
///
/// Step transitions are written as they happen, with no lock held, so
/// another pod can follow a run in progress.
///
/// Migration events are buffered instead. They arrive from inside the
/// migrate lock (see [`crate::migrate::progress`]), and awaiting a
/// registry write there would hold that lock while other pods queue
/// behind it. They are flushed when the migrate step ends.
///
/// So a watcher sees `Migrate: started`, a pause, then the whole
/// per-migration log at once.
pub(super) struct Reporter<'a> {
    observer: Option<&'a dyn ProvisionObserver>,
    store: Option<RunStore<'a>>,
    /// Pick up a tenant a failed run left inactive, instead of refusing
    /// its slug (#1883).
    resume: bool,
}

struct RunStore<'a> {
    registry: &'a crate::sql::Pool,
    run_id: i64,
    /// Names the run in the log line of a withheld failure.
    slug: String,
    log: super::provision_store::RunLog,
    /// The operator-safe text of the last failure, for the run's `error`.
    failure: std::sync::Mutex<Option<RunText>>,
    /// Next `seq`. Dense and per-run, because `Last-Event-ID` counts
    /// from it.
    seq: std::sync::atomic::AtomicI64,
    buffered: std::sync::Mutex<Vec<(&'static str, &'static str, RunText)>>,
}

/// A step transition the store may record. No `Failed`: a failure goes
/// through [`Reporter::fail`], which withholds its cause (#2212).
enum Progress {
    Started,
    Ok,
    Skipped(&'static str),
}

impl Progress {
    fn status(&self) -> StepStatus {
        match self {
            Self::Started => StepStatus::Started,
            Self::Ok => StepStatus::Ok,
            Self::Skipped(why) => StepStatus::Skipped((*why).to_owned()),
        }
    }
}

impl<'a> Reporter<'a> {
    pub(super) fn new(observer: Option<&'a dyn ProvisionObserver>) -> Self {
        Self {
            observer,
            store: None,
            resume: false,
        }
    }

    /// Also persist to `run_id` in the registry.
    pub(super) fn persisting(
        mut self,
        registry: &'a crate::sql::Pool,
        run_id: i64,
        slug: &str,
    ) -> Self {
        self.store = Some(RunStore {
            registry,
            run_id,
            slug: slug.to_owned(),
            log: super::provision_store::RunLog::new(run_id),
            failure: std::sync::Mutex::new(None),
            seq: std::sync::atomic::AtomicI64::new(1),
            buffered: std::sync::Mutex::new(Vec::new()),
        });
        self
    }

    /// Link the new Org to this run as soon as it exists, so a retry can
    /// tell which run made it.
    async fn registered(&self, org_id: i64) {
        let Some(store) = &self.store else {
            return;
        };
        let attach = || super::provision_store::attach_org(store.registry, store.run_id, org_id);
        // One retry: this is the run's only org link (#2061).
        if let Err(e) = match attach().await {
            Err(_) => attach().await,
            ok => ok,
        } {
            tracing::warn!(target: "rustango::tenancy::provision", error = %e, "could not attach org id to run");
        }
    }

    fn notify(&self, event: ProvisionEvent) {
        if let Some(observer) = self.observer {
            observer.on_event(event);
        }
    }

    /// Write one row now. Failures are logged, never propagated: the
    /// record of a provisioning run must not be what fails it.
    async fn write(&self, step: &str, status: &str, message: &RunText) {
        let Some(store) = &self.store else {
            return;
        };
        let seq = store.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = super::provision_store::append_event_text(
            store.registry,
            store.run_id,
            seq,
            step,
            status,
            message,
        )
        .await
        {
            tracing::warn!(
                target: "rustango::tenancy::provision",
                run_id = store.run_id,
                error = %e,
                "could not record a provisioning event; the run continues"
            );
        }
    }

    /// Queue a migration event for the next [`flush`](Self::flush).
    fn buffer(&self, step: &'static str, status: &'static str, message: RunText) {
        let Some(store) = &self.store else {
            return;
        };
        if let Ok(mut buf) = store.buffered.lock() {
            buf.push((step, status, message));
        }
    }

    /// Write everything buffered, in order.
    async fn flush(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let pending = match store.buffered.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => return,
        };
        for (step, status, message) in pending {
            self.write(step, status, &message).await;
        }
    }
}

impl Reporter<'_> {
    /// Announce a step transition — to the observer, and to the store.
    async fn step(&self, step: ProvisionStep, progress: Progress) {
        let status = progress.status();
        let text = match progress {
            Progress::Skipped(why) => RunText::fixed(why),
            Progress::Started | Progress::Ok => RunText::default(),
        };
        self.write(step.as_str(), status.as_str(), &text).await;
        self.notify(ProvisionEvent::Step { step, status });
    }

    /// Report a step's failure, then hand back the error.
    ///
    /// Every failure path goes through this rather than a bare `?`, so
    /// a watcher never sees a step stuck on `Started` with no
    /// explanation — which is the state the whole event stream exists
    /// to prevent.
    async fn fail<T>(&self, at: ProvisionStep, e: TenancyError) -> Result<T, TenancyError> {
        let text = self
            .store
            .as_ref()
            .map(|store| store.log.failure(&store.slug, "Step failed", &e));
        self.step_failed(at, e.to_string(), text).await;
        Err(e)
    }

    /// A failed migrate step: the cause was a migration, already logged once.
    async fn migrate_failed(&self, cause: &str) {
        let text = self
            .store
            .as_ref()
            .map(|store| store.log.withheld(&store.slug, "Step failed", &cause));
        self.step_failed(ProvisionStep::Migrate, cause.to_owned(), text)
            .await;
    }

    /// The observer gets `cause`; the store gets `text` (#2198).
    async fn step_failed(&self, step: ProvisionStep, cause: String, text: Option<RunText>) {
        if let (Some(store), Some(text)) = (&self.store, text) {
            self.write(step.as_str(), "failed", &text).await;
            if let Ok(mut last) = store.failure.lock() {
                *last = Some(text);
            }
        }
        self.notify(ProvisionEvent::Step {
            step,
            status: StepStatus::Failed(cause),
        });
    }

    /// The run's stored `error`: the last failed step's text, else `e`'s.
    fn run_error(&self, e: Option<&TenancyError>) -> Option<RunText> {
        let store = self.store.as_ref()?;
        let last = store.failure.lock().ok().and_then(|f| f.clone());
        match e {
            Some(e) => Some(
                RunText::user_facing(e)
                    .or(last)
                    .unwrap_or_else(|| store.log.failure(&store.slug, "Provisioning failed", e)),
            ),
            None => last,
        }
    }

    /// A migration event from the tenant's own run. Buffered rather
    /// than written — see the type docs.
    fn migration(&self, event: tenant_migrate::TenantMigrationEvent) {
        if let Some(store) = &self.store {
            let (step, status, text) = store.log.migration(&event);
            self.buffer(step, status, text);
        }
        self.notify(ProvisionEvent::Migration(event));
    }
}

impl ProvisionStep {
    /// Stable wire name, stored in `rustango_provisioning_events.step`
    /// and matched on by a console. Written out rather than derived
    /// from `Debug`, which is not a stable format.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::CheckConnection => "check_connection",
            Self::ProvisionStorage => "provision_storage",
            Self::RegisterOrg => "register_org",
            Self::Migrate => "migrate",
            Self::Activate => "activate",
        }
    }
}

impl StepStatus {
    /// Stable wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ok => "ok",
            Self::Skipped(_) => "skipped",
            Self::Failed(_) => "failed",
        }
    }

    /// The reason carried by `Skipped` / `Failed`, or empty.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Started | Self::Ok => "",
            Self::Skipped(why) | Self::Failed(why) => why,
        }
    }
}

/// Stand up a tenant: validate, provision its storage, register it, and
/// migrate it.
///
/// The CLI verb is a thin wrapper over this, as is anything else that
/// creates a tenant.
///
/// # Errors
/// `Err` if the slug is taken, the request contradicts itself (database
/// mode with no URL, schema mode on a non-PG build), the schema could
/// not be created, or the row could not be inserted.
///
/// A migration failure is not an error: the tenant already exists, so
/// it comes back as [`MigrationsOutcome::Failed`] in
/// [`ProvisionOutcome::migrations`].
pub async fn provision_tenant<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    request: &ProvisionRequest,
    observer: Option<&dyn ProvisionObserver>,
) -> Result<ProvisionOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let rep = Reporter::new(observer);
    provision_reported(pools, registry_url, dir, request, &rep).await
}

/// [`provision_tenant`], with the run persisted to
/// [`super::provision_store`] as it goes.
///
/// Step transitions are written as they happen, so another pod can
/// read a run, or replay it from the start.
///
/// # Errors
/// As [`provision_tenant`]. A failure to *record* the run is only
/// logged: bookkeeping must not fail a tenant creation.
pub async fn provision_tenant_recorded<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    request: &ProvisionRequest,
    observer: Option<&dyn ProvisionObserver>,
    requested_by: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<(super::provision_store::ProvisioningRun, ProvisionOutcome), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use super::provision_store as store;

    let registry = pools.registry_pool();
    let run = store::open_run(
        &registry,
        &request.slug,
        request.mode.as_str(),
        request.backend.as_str(),
        request.database_url.as_deref(),
        requested_by,
        idempotency_key,
    )
    .await?;
    let run_id = run.id.get().copied().unwrap_or_default();

    let outcome = in_run(pools, registry_url, dir, request, observer, run_id, false)
        .await
        // A pre-row failure still closed the run inside
        // `provision_tenant_in_run`; propagate the reason unchanged.
        ?;
    // Re-read so the caller sees the closed run, not the one that was
    // handed back at `open_run` time.
    let refreshed = store::run_by_id(&registry, run_id).await?.unwrap_or(run);
    Ok((refreshed, outcome))
}

/// [`provision_tenant_recorded`] against a run somebody else already
/// opened.
///
/// The inbound webhook needs this. It opens the run first, so the
/// response can carry the id and any `idempotency_key` conflict, then
/// hands the slow part to a task. Without this the task would open a
/// second run for the same tenant.
///
/// A retry resumes: if the slug's Org was made by a provision run that
/// failed and is still inactive, it is migrated and activated instead of
/// refused as taken (#1883).
///
/// # Errors
/// As [`provision_tenant`]. The run is closed either way.
pub async fn provision_tenant_in_run<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    request: &ProvisionRequest,
    observer: Option<&dyn ProvisionObserver>,
    run_id: i64,
) -> Result<ProvisionOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    in_run(pools, registry_url, dir, request, observer, run_id, true).await
}

async fn in_run<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    request: &ProvisionRequest,
    observer: Option<&dyn ProvisionObserver>,
    run_id: i64,
    resume: bool,
) -> Result<ProvisionOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use super::provision_store::{self as store, RunState};

    let registry = pools.registry_pool();
    let mut rep = Reporter::new(observer).persisting(&registry, run_id, &request.slug);
    rep.resume = resume;
    let result = provision_reported(pools, registry_url, dir, request, &rep).await;

    // Close the run whatever happened, error paths included. A run
    // stuck at `running` is the ambiguity this table removes.
    // The stored error is operator-safe; the failed step logged the cause (#2198).
    let (state, error) = match &result {
        Ok(outcome) => match &outcome.migrations {
            MigrationsOutcome::Failed(_) => (RunState::Failed, rep.run_error(None)),
            _ => (RunState::Succeeded, None),
        },
        Err(e) => (RunState::Failed, rep.run_error(Some(e))),
    };
    // No `attach_org` here: `Reporter::registered` linked the org already.
    if let Err(e) = store::finish_run_text(&registry, run_id, state, error.as_ref()).await {
        tracing::warn!(target: "rustango::tenancy::provision", error = %e, "could not close provisioning run");
    }

    result
}

async fn provision_reported<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    request: &ProvisionRequest,
    rep: &Reporter<'_>,
) -> Result<ProvisionOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // ---- 1. Validate ----
    rep.step(ProvisionStep::Validate, Progress::Started).await;
    let registry = pools.registry_pool();

    // Reject a duplicate slug up front — it saves a partial-state mess
    // where `CREATE SCHEMA` succeeds and the `INSERT` then fails.
    let existing: Vec<Org> = match Org::objects()
        .where_(Org::slug.eq(request.slug.clone()))
        .fetch(&registry)
        .await
    {
        Ok(rows) => rows,
        Err(e) => return rep.fail(ProvisionStep::Validate, e.into()).await,
    };
    if let Some(org) = existing.into_iter().next() {
        let resumable = rep.resume
            && !org.active
            && org.storage_mode == request.mode.as_str()
            && match super::provision_store::org_left_by_failed_run(
                &registry,
                org.id.get().copied().unwrap_or_default(),
            )
            .await
            {
                Ok(left) => left,
                Err(e) => return rep.fail(ProvisionStep::Validate, e).await,
            };
        if !resumable {
            return rep
                .fail(
                    ProvisionStep::Validate,
                    TenancyError::Validation(format!(
                        "tenant slug `{}` already exists",
                        request.slug
                    )),
                )
                .await;
        }
        rep.step(ProvisionStep::Validate, Progress::Ok).await;
        rep.step(
            ProvisionStep::RegisterOrg,
            Progress::Skipped("resuming the tenant an earlier run left inactive"),
        )
        .await;
        let org_id = org.id.get().copied().unwrap_or_default();
        rep.registered(org_id).await;
        return migrate_and_activate(pools, registry_url, dir, request, &org, rep).await;
    }

    let normalized = match checked_request(&registry, registry_url, request).await {
        Ok(r) => r,
        Err(e) => return rep.fail(ProvisionStep::Validate, e).await,
    };
    let request = &normalized;

    if request.mode == StorageMode::Database && request.database_url.is_none() {
        return rep
            .fail(
                ProvisionStep::Validate,
                TenancyError::Validation("database mode needs a database URL".into()),
            )
            .await;
    }

    rep.step(ProvisionStep::Validate, Progress::Ok).await;

    // ---- 2. Check the connection ----
    //
    // Before anything is written. A database-mode URL that is wrong —
    // typo'd host, wrong password, database not created yet — used to
    // be discovered *after* the `Org` row landed, leaving a tenant the
    // resolver matches in front of a database with no schema.
    check_connection(request, rep).await?;

    let schema_name = schema_name_for(request);

    // ---- 3. Provision storage ----
    //
    // Before the row, not after: a failed `INSERT` must not leave an
    // orphan schema behind. An existing schema is refused (#2394).
    provision_storage(pools, schema_name.as_deref(), rep).await?;

    // ---- 4. Register the org ----
    rep.step(ProvisionStep::RegisterOrg, Progress::Started)
        .await;
    let mut org = new_org_row(request, schema_name);
    if let Err(e) = super::org_host::insert_org(&registry, &mut org).await {
        if let Some(schema) = org.schema_name.as_deref() {
            release_schema(pools, schema).await;
        }
        return rep.fail(ProvisionStep::RegisterOrg, e).await;
    }
    // This pod sees the new tenant immediately; others converge on the
    // registry fingerprint (see `resolver::sync_org_generation`).
    super::invalidate_org_cache();
    let org_id = org.id.get().copied().unwrap_or_default();
    rep.registered(org_id).await;
    rep.step(ProvisionStep::RegisterOrg, Progress::Ok).await;
    rep.notify(ProvisionEvent::Registered { org_id });

    migrate_and_activate(pools, registry_url, dir, request, &org, rep).await
}

/// Steps 5 and 6, shared by a fresh tenant and a resumed one.
async fn migrate_and_activate<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    request: &ProvisionRequest,
    org: &Org,
    rep: &Reporter<'_>,
) -> Result<ProvisionOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let org_id = org.id.get().copied().unwrap_or_default();

    // ---- 5. Migrate ----
    let migrations = if request.run_migrations {
        rep.step(ProvisionStep::Migrate, Progress::Started).await;
        let outcome = migrate_new_tenant(pools, registry_url, dir, org, rep).await;
        // The migration events arrived synchronously while the migrate
        // lock was held, so they were buffered. The lock is released by
        // now — write them before reporting the step's own outcome, so
        // the stored log reads in the order things happened.
        rep.flush().await;
        match &outcome {
            MigrationsOutcome::Failed(e) => rep.migrate_failed(e).await,
            _ => rep.step(ProvisionStep::Migrate, Progress::Ok).await,
        }
        outcome
    } else {
        rep.step(
            ProvisionStep::Migrate,
            Progress::Skipped("caller asked for no migrations"),
        )
        .await;
        MigrationsOutcome::Skipped
    };

    // ---- 6. Activate ----
    //
    // The row went in inactive, so until now the tenant does not
    // resolve. A migration failure leaves it that way on purpose: the
    // row stays so an operator can see what was half-made, but nothing
    // routes to it.
    if matches!(migrations, MigrationsOutcome::Failed(_)) {
        rep.step(
            ProvisionStep::Activate,
            Progress::Skipped("migrations failed; the tenant stays inactive"),
        )
        .await;
    } else {
        rep.step(ProvisionStep::Activate, Progress::Started).await;
        if let Err(e) = activate(&registry, org_id).await {
            return rep.fail(ProvisionStep::Activate, e).await;
        }
        super::invalidate_org_cache();
        rep.step(ProvisionStep::Activate, Progress::Ok).await;
    }

    Ok(ProvisionOutcome {
        org_id,
        slug: request.slug.clone(),
        mode: request.mode,
        migrations,
    })
}

/// Reach the tenant's own database before anything is written.
///
/// Only database-mode has a separate database to reach; a schema-mode
/// tenant lives in the registry's, which is already connected.
async fn check_connection(
    request: &ProvisionRequest,
    rep: &Reporter<'_>,
) -> Result<(), TenancyError> {
    let (Some(url), StorageMode::Database) = (&request.database_url, request.mode) else {
        rep.step(
            ProvisionStep::CheckConnection,
            Progress::Skipped("schema-mode tenants share the registry's database"),
        )
        .await;
        return Ok(());
    };

    rep.step(ProvisionStep::CheckConnection, Progress::Started)
        .await;
    match preflight::check(url, &request.preflight).await {
        Ok(_) => {
            rep.step(ProvisionStep::CheckConnection, Progress::Ok).await;
            Ok(())
        }
        // `Validation`, not a driver error: the supplied URL is wrong,
        // and the diagnosis already says what to change.
        // The driver's own words are logged, not stored or shown (#2212).
        Err(d) => {
            tracing::warn!(
                target: "rustango::tenancy::provision",
                org = %request.slug,
                detail = %d.detail,
                "tenant connection check failed"
            );
            rep.fail(
                ProvisionStep::CheckConnection,
                TenancyError::Validation(d.summary()),
            )
            .await
        }
    }
}

/// Make the tenant's storage, if it needs making.
///
/// Schema-mode gets a `CREATE SCHEMA`; database-mode brings its own
/// database, already reached by the connection check. Deliberately
/// before the `Org` row lands, so a failed `INSERT` leaves no orphan
/// schema.
async fn provision_storage<DB: Database>(
    pools: &TenantPools<DB>,
    schema_name: Option<&str>,
    rep: &Reporter<'_>,
) -> Result<(), TenancyError> {
    // `schema_name_for` is `Some` exactly in schema mode.
    let Some(schema) = schema_name else {
        rep.step(
            ProvisionStep::ProvisionStorage,
            Progress::Skipped("database-mode tenants bring their own database"),
        )
        .await;
        return Ok(());
    };

    rep.step(ProvisionStep::ProvisionStorage, Progress::Started)
        .await;
    if let Err(e) = provision_schema(pools, schema).await {
        return rep.fail(ProvisionStep::ProvisionStorage, e).await;
    }
    rep.step(ProvisionStep::ProvisionStorage, Progress::Ok)
        .await;
    Ok(())
}

/// A slug becomes a database name, a schema name and a hostname label
/// (`<slug>.<apex>`), so it must be legal as all three.
///
/// The hostname label is strictest: RFC 1123 allows lowercase letters,
/// digits and hyphens only, and no hyphen at either end. A slug with a
/// space would give a live tenant nobody can reach.
fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("a slug is required".into());
    }
    if slug.len() > 63 {
        // The hostname-label limit; the database-name limits are
        // higher, so this is the binding one.
        return Err(format!(
            "slug `{slug}` is {} characters; a hostname label allows at most 63",
            slug.len()
        ));
    }
    let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    if let Some(bad) = slug.bytes().find(|b| !legal(*b)) {
        return Err(format!(
            "slug `{slug}` contains `{}` — only lowercase letters, digits and hyphens are \
             allowed, because the slug becomes a database name, a schema name and a \
             hostname label",
            bad as char
        ));
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err(format!(
            "slug `{slug}` may not start or end with a hyphen — a hostname label cannot"
        ));
    }
    Ok(())
}

/// Check every operator-supplied field and return the request with the
/// host pattern normalized.
///
/// One function, called from the `Validate` step, so the console, the
/// webhook and `manage create-tenant` agree on what a legal tenant is.
///
/// Each field feeds an exact matcher or identifier. A value the matcher
/// can never produce gives a tenant that is registered, active and
/// unreachable, so it is refused here.
fn validate_fields(request: &ProvisionRequest) -> Result<ProvisionRequest, String> {
    validate_slug(&request.slug)?;

    // The effective name, so the slug-derived default is checked by the
    // same rule as an explicit one.
    if let Some(schema) = schema_name_for(request) {
        validate_schema_name(&schema)?;
    }
    if let Some(prefix) = &request.path_prefix {
        validate_path_prefix(prefix)?;
    }
    if let Some(port) = request.port {
        validate_port(port)?;
    }

    let mut out = request.clone();
    // The `<slug>.<APEX>` default is filled in here so it is validated too.
    let pattern = request.host_pattern.clone().or_else(|| {
        crate::tenancy::server::configured_apex_domain()
            .map(|apex| format!("{}.{apex}", request.slug))
    });
    out.host_pattern = pattern.as_deref().map(validate_host_pattern).transpose()?;
    Ok(out)
}

/// The schema a schema-mode tenant is created in.
///
/// The name reaches `CREATE SCHEMA` and `SET search_path` as an
/// identifier. [`crate::sql::Dialect::quote_ident`] quotes it, so odd
/// characters cannot execute, but a schema nobody can name again
/// without quoting is a tenant nobody can operate. So reject it here.
///
/// The rule is the slug's, plus underscores: a schema name is not a
/// hostname label, and Postgres is happy with `_`.
pub(crate) fn validate_schema_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a schema name is required".into());
    }
    if name.len() > 63 {
        // Postgres truncates identifiers at NAMEDATALEN-1 silently,
        // which would make the stored name and the real schema differ.
        return Err(format!(
            "schema name `{name}` is {} characters; Postgres allows at most 63",
            name.len()
        ));
    }
    let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-';
    if let Some(bad) = name.bytes().find(|b| !legal(*b)) {
        return Err(format!(
            "schema name `{name}` contains `{}` — only lowercase letters, digits, underscores \
             and hyphens are allowed",
            bad as char
        ));
    }
    // A leading digit is fine, because the default schema name is the
    // slug and a slug may start with one. Only a leading hyphen is
    // refused, since it reads as a flag.
    if name.starts_with('-') {
        return Err(format!("schema name `{name}` may not start with a hyphen"));
    }
    // `pg_*` is reserved. Catch it here so it reads as a validation
    // error rather than a failed run.
    if name.starts_with("pg_") || name == "information_schema" {
        return Err(format!(
            "schema name `{name}` is reserved by Postgres — choose another"
        ));
    }
    // `public` holds the registry tables and ends every tenant's
    // `search_path`, so a tenant there would share both.
    if name == "public" {
        return Err(format!(
            "schema name `{name}` is the registry's schema — choose another"
        ));
    }
    Ok(())
}

/// The `Host` header this tenant answers to.
///
/// The resolver lowercases the header and strips the port, then
/// compares exactly. So uppercase, a `:port` suffix or a wildcard all
/// give a tenant that can never be reached.
///
/// Uppercase is lowercased here, since that is what the resolver does
/// anyway. A port or a wildcard is refused: the operator wants
/// something this matcher cannot do.
pub(crate) fn validate_host_pattern(pattern: &str) -> Result<String, String> {
    if pattern.contains(':') {
        return Err(format!(
            "host pattern `{pattern}` carries a port — the `Host` header is matched with the \
             port stripped, so this could never match. Put the port in the port field"
        ));
    }
    if pattern.contains('*') {
        return Err(format!(
            "host pattern `{pattern}` uses a wildcard — patterns are matched exactly, so this \
             could never match. Register one tenant per hostname"
        ));
    }
    if pattern.len() > 253 {
        return Err(format!(
            "host pattern `{pattern}` is {} characters; a hostname allows at most 253",
            pattern.len()
        ));
    }
    let normalized = pattern.to_ascii_lowercase();
    for label in normalized.split('.') {
        if label.is_empty() {
            return Err(format!(
                "host pattern `{pattern}` has an empty label — check for a doubled or trailing dot"
            ));
        }
        if label.len() > 63 {
            return Err(format!(
                "host pattern `{pattern}` has a label longer than the 63 characters a hostname \
                 allows"
            ));
        }
        let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if let Some(bad) = label.bytes().find(|b| !legal(*b)) {
            return Err(format!(
                "host pattern `{pattern}` contains `{}` — a hostname allows only letters, \
                 digits, hyphens and dots",
                bad as char
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "host pattern `{pattern}` has a label starting or ending with a hyphen, which a \
                 hostname cannot"
            ));
        }
    }
    Ok(normalized)
}

/// The URL path segment this tenant answers to.
///
/// `PathPrefixResolver` takes the **first** path segment and looks up
/// `"/<segment>"`. So a stored prefix that is not exactly one
/// leading-slash segment — no slash, a second segment, a trailing slash
/// — cannot be produced by that lookup and never matches.
pub(crate) fn validate_path_prefix(prefix: &str) -> Result<(), String> {
    let Some(segment) = prefix.strip_prefix('/') else {
        return Err(format!(
            "path prefix `{prefix}` must start with `/` — the resolver looks up `/<segment>`"
        ));
    };
    if segment.is_empty() {
        return Err("path prefix `/` is the apex, which resolves to no tenant".into());
    }
    if segment.contains('/') {
        return Err(format!(
            "path prefix `{prefix}` has more than one segment — only the first segment of the \
             URL is matched, so this could never match"
        ));
    }
    if segment.bytes().all(|b| b == b'.') {
        return Err(format!(
            "path prefix `{prefix}` is a relative-path segment, not a tenant prefix"
        ));
    }
    let legal = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
    if let Some(bad) = segment.bytes().find(|b| !legal(*b)) {
        return Err(format!(
            "path prefix `{prefix}` contains `{}` — a path segment that needs escaping would \
             not match the decoded path",
            bad as char
        ));
    }
    Ok(())
}

/// The TCP port this tenant answers on.
///
/// `i32` is the column's type, not the range of a port. `-1` and
/// `999999` both parsed and both stored, producing a tenant matched
/// against a port no listener can ever have.
pub(crate) fn validate_port(port: i32) -> Result<(), String> {
    if !(1..=65535).contains(&port) {
        return Err(format!(
            "port {port} is outside the 1–65535 a TCP port can be"
        ));
    }
    Ok(())
}

/// Every free-text field, the registry-URL refusal, then the routing clash check. Returns the
/// request with `host_pattern` normalized; shared with `api::create_tenant` (#2097).
pub(crate) async fn checked_request(
    registry: &crate::sql::Pool,
    registry_url: &str,
    request: &ProvisionRequest,
) -> Result<ProvisionRequest, TenancyError> {
    let normalized = validate_fields(request).map_err(TenancyError::Validation)?;
    if let Some(url) = &normalized.database_url {
        refuse_registry(url, registry, registry_url).map_err(TenancyError::Validation)?;
    }
    if let Some(clash) = routing_clash(registry, &normalized).await? {
        return Err(TenancyError::Validation(format!(
            "{clash} is already used by another tenant"
        )));
    }
    Ok(normalized)
}

/// The schema, host, path prefix or port of `request` another tenant uses.
async fn routing_clash(
    registry: &crate::sql::Pool,
    request: &ProvisionRequest,
) -> Result<Option<String>, crate::sql::ExecError> {
    use super::org_host::{host_claimed, port_claimed, prefix_claimed, schema_claimed};
    if let Some(schema) = schema_name_for(request) {
        if schema_claimed(registry, &schema, None).await? {
            return Ok(Some(format!("schema `{schema}`")));
        }
    }
    if let Some(host) = &request.host_pattern {
        if host_claimed(registry, host, None).await? {
            return Ok(Some(format!("host `{host}`")));
        }
    }
    if let Some(prefix) = &request.path_prefix {
        if prefix_claimed(registry, prefix, None).await? {
            return Ok(Some(format!("path prefix `{prefix}`")));
        }
    }
    if let Some(port) = request.port {
        if port_claimed(registry, port, None).await? {
            return Ok(Some(format!("port {port}")));
        }
    }
    Ok(None)
}

/// Refuse a tenant URL that points at the registry's own database.
///
/// Otherwise provisioning runs the *tenant* migration chain into the
/// *registry*, creating tenant tables there and writing entries into
/// the registry's own ledger. Any destructive tenant migration would
/// then run against the registry.
///
/// Compares the [`Endpoint`] only, because two URLs may name the same
/// database with different credentials.
pub(crate) fn refuse_registry_url(tenant_url: &str, registry_url: &str) -> Result<(), String> {
    let Some(kind) = Endpoint::scheme_kind(registry_url) else {
        return Ok(());
    };
    match Endpoint::parse(registry_url, kind) {
        Ok(registry) => refuse_endpoint(tenant_url, kind, &registry),
        Err(_) => Ok(()),
    }
}

/// Both refusals: the pool's endpoint, then the configured registry URL.
pub(crate) fn refuse_registry(
    tenant_url: &str,
    registry: &crate::sql::Pool,
    registry_url: &str,
) -> Result<(), String> {
    refuse_registry_pool(tenant_url, registry)
        .and_then(|()| refuse_registry_url(tenant_url, registry_url))
}

/// [`refuse_registry_url`] against the database the registry pool is
/// connected to (#2320).
pub(crate) fn refuse_registry_pool(
    tenant_url: &str,
    registry: &crate::sql::Pool,
) -> Result<(), String> {
    let (kind, endpoint) = Endpoint::of_pool(registry);
    refuse_endpoint(tenant_url, kind, &endpoint)
}

fn refuse_endpoint(tenant_url: &str, kind: BackendKind, registry: &Endpoint) -> Result<(), String> {
    let shown = crate::sql::connect_diagnosis::redact(tenant_url);
    let tenant = Endpoint::of_url(tenant_url, kind).map_err(|e| {
        format!("cannot read {shown} ({e}), so cannot tell it from the registry's own database")
    })?;
    if tenant.as_ref() == Some(registry) {
        return Err(format!(
            "this is the registry's own database ({shown}). A tenant needs its own — pointing one \
             here would run the tenant migrations over the registry"
        ));
    }
    Ok(())
}

/// Where a connection lands, as the backend's sqlx options read it, so
/// `?host=`, sockets, `PG*` env and default ports resolve as the pool did (#2320).
#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    Server {
        place: Place,
        port: u16,
        database: Option<String>,
    },
    #[cfg(feature = "sqlite")]
    Sqlite(std::path::PathBuf),
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// Lowercased.
    Host(String),
    Socket(std::path::PathBuf),
}

impl Endpoint {
    fn scheme_kind(url: &str) -> Option<BackendKind> {
        BackendKind::parse(&url.split(':').next()?.to_ascii_lowercase()).ok()
    }

    /// Read as the registry's backend connects it: sqlx ignores a PG/MySQL scheme and SQLite
    /// takes a bare path. `Ok(None)` when another scheme fails to parse, e.g. a secret reference.
    fn of_url(url: &str, kind: BackendKind) -> Result<Option<Self>, String> {
        match Self::parse(url, kind) {
            Ok(endpoint) => Ok(Some(endpoint)),
            Err(e) if Self::scheme_kind(url) == Some(kind) => Err(e),
            Err(_) => Ok(None),
        }
    }

    fn parse(url: &str, kind: BackendKind) -> Result<Self, String> {
        match kind {
            #[cfg(feature = "postgres")]
            BackendKind::Postgres => url
                .parse::<sqlx::postgres::PgConnectOptions>()
                .map(|o| Self::of_pg(&o))
                .map_err(|e| e.to_string()),
            #[cfg(feature = "mysql")]
            BackendKind::MySql => url
                .parse::<sqlx::mysql::MySqlConnectOptions>()
                .map(|o| Self::of_mysql(&o))
                .map_err(|e| e.to_string()),
            #[cfg(feature = "sqlite")]
            BackendKind::Sqlite => url
                .parse::<sqlx::sqlite::SqliteConnectOptions>()
                .map(|o| Self::Sqlite(sqlite_file(o.get_filename())))
                .map_err(|e| e.to_string()),
            #[allow(unreachable_patterns)]
            other => Err(format!("this build has no {} backend", other.as_str())),
        }
    }

    fn of_pool(pool: &crate::sql::Pool) -> (BackendKind, Self) {
        match pool {
            #[cfg(feature = "postgres")]
            crate::sql::Pool::Postgres(p) => {
                (BackendKind::Postgres, Self::of_pg(&p.connect_options()))
            }
            #[cfg(feature = "mysql")]
            crate::sql::Pool::Mysql(p) => {
                (BackendKind::MySql, Self::of_mysql(&p.connect_options()))
            }
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(p) => (
                BackendKind::Sqlite,
                Self::Sqlite(sqlite_file(p.connect_options().get_filename())),
            ),
        }
    }

    /// No database means the server picks the user's name.
    #[cfg(feature = "postgres")]
    fn of_pg(o: &sqlx::postgres::PgConnectOptions) -> Self {
        Self::Server {
            place: Place::of(o.get_socket(), o.get_host()),
            port: o.get_port(),
            database: Some(o.get_database().unwrap_or(o.get_username()).to_owned()),
        }
    }

    /// A MySQL socket is the whole address; the port is unused.
    #[cfg(feature = "mysql")]
    fn of_mysql(o: &sqlx::mysql::MySqlConnectOptions) -> Self {
        Self::Server {
            port: if o.get_socket().is_some() {
                0
            } else {
                o.get_port()
            },
            place: Place::of(o.get_socket(), o.get_host()),
            database: o.get_database().map(str::to_owned),
        }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl Place {
    /// sqlx keeps a default or `PGHOST` socket dir in `host`, a `?host=/dir` one in `socket`.
    fn of(socket: Option<&std::path::PathBuf>, host: &str) -> Self {
        match socket {
            Some(s) => Self::Socket(file_identity(s)),
            None if host.starts_with('/') => Self::Socket(file_identity(Path::new(host))),
            None => {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                Self::Host(host.trim_end_matches('.').to_ascii_lowercase())
            }
        }
    }
}

/// The file SQLite opens: a `file:` URI names its path, percent-encoded, before `?`.
#[cfg(feature = "sqlite")]
fn sqlite_file(name: &Path) -> std::path::PathBuf {
    let name = name.to_string_lossy();
    let Some(uri) = name.strip_prefix("file:") else {
        return file_identity(Path::new(&*name));
    };
    let uri = uri.split(['?', '#']).next().unwrap_or_default();
    // `file://host/path`: only an empty or `localhost` authority is legal.
    let path = match uri.strip_prefix("//") {
        Some(rest) => rest.find('/').map_or("", |i| &rest[i..]),
        None => uri,
    };
    file_identity(Path::new(&crate::url_codec::percent_decode_path(path)))
}

/// The file `path` opens, walked as SQLite's unix VFS does: `..` pops by
/// text, and each component that exists has its symlinks resolved.
fn file_identity(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let abs = std::env::current_dir().map_or_else(|_| path.to_owned(), |d| d.join(path));
    let mut resolved = std::path::PathBuf::new();
    for part in abs.components() {
        match part {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => {
                resolved.push(other);
                if let Ok(real) = std::fs::canonicalize(&resolved) {
                    resolved = real;
                }
            }
        }
    }
    resolved
}

/// Build a tenant URL on the same server as the registry, naming
/// `database`.
///
/// For the common "one Postgres, one database per tenant" setup. The
/// caller supplies a database *name*, not a URL, so the registry
/// password never reaches a page or a form post.
///
/// Returns `None` when the registry URL has no database segment to
/// replace; the operator should then supply a full URL.
#[must_use]
pub fn tenant_url_on_registry_server(registry_url: &str, database: &str) -> Option<String> {
    // sqlite is a file path, not a server: "same server, other
    // database" means a sibling file.
    if let Some(path) = registry_url.strip_prefix("sqlite://") {
        let path = path.split('?').next().unwrap_or(path);
        // An in-memory registry has no directory to put a sibling in.
        if path.is_empty() || path.starts_with(":memory:") {
            return None;
        }
        // A bare filename is relative to the working directory, which
        // is where the sibling belongs.
        let dir = path.rsplit_once('/').map_or(".", |(d, _)| d);
        // A sqlite database is a file, so `acme` becomes `acme.db` and
        // `acme.db` is left as-is. The check ignores case: `acme.DB` is
        // the same file on a case-insensitive filesystem.
        let file = if std::path::Path::new(database)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("db"))
        {
            database.to_owned()
        } else {
            format!("{database}.db")
        };
        return Some(format!("sqlite://{dir}/{file}?mode=rwc"));
    }

    let (scheme, rest) = registry_url.split_once("://")?;
    // Split the query off first: it can hold a `/` (`sslrootcert=/ca.pem`)
    // and its TLS options must carry over to the tenant.
    let (rest, query) = match rest.split_once('?') {
        Some((path, q)) => (path, Some(q)),
        None => (rest, None),
    };
    // Keep userinfo and authority; replace only the path segment.
    let (authority, _old_db) = rest.rsplit_once('/')?;
    if authority.is_empty() {
        return None;
    }
    // Only TLS keys carry over: sqlx lets `dbname`/`host`/`user` in the
    // query override the URL, so keeping them could point at the registry.
    let tls: Vec<&str> = query
        .unwrap_or("")
        .split('&')
        .filter(|chunk| {
            let key = chunk.split_once('=').map_or(*chunk, |(k, _)| k);
            TLS_QUERY_KEYS.contains(&crate::url_codec::url_decode(key).as_str())
        })
        .collect();
    Some(if tls.is_empty() {
        format!("{scheme}://{authority}/{database}")
    } else {
        format!("{scheme}://{authority}/{database}?{}", tls.join("&"))
    })
}

/// The TLS query keys sqlx 0.8 reads for Postgres and MySQL.
const TLS_QUERY_KEYS: &[&str] = &[
    "sslmode",
    "ssl-mode",
    "sslrootcert",
    "ssl-root-cert",
    "sslca",
    "ssl-ca",
    "sslcert",
    "ssl-cert",
    "sslkey",
    "ssl-key",
];

/// The schema a schema-mode tenant lives in: whatever the request
/// named, or the slug. `None` in database-mode, which has no schema.
pub(crate) fn schema_name_for(request: &ProvisionRequest) -> Option<String> {
    match request.mode {
        StorageMode::Schema => Some(
            super::org::effective_schema(request.schema_name.as_deref(), &request.slug).to_owned(),
        ),
        StorageMode::Database => None,
    }
}

/// The registry row for a validated request, slug-derived defaults
/// filled in.
fn new_org_row(request: &ProvisionRequest, schema_name: Option<String>) -> Org {
    Org {
        id: Auto::default(),
        slug: request.slug.clone(),
        display_name: request
            .display_name
            .clone()
            .unwrap_or_else(|| request.slug.clone()),
        storage_mode: request.mode.as_str().into(),
        backend_kind: request.backend.as_str().into(),
        database_url: request.database_url.clone(),
        schema_name,
        // `validate_fields` already filled in the `<slug>.<APEX>` default.
        host_pattern: request.host_pattern.clone(),
        port: request.port,
        path_prefix: request.path_prefix.clone(),
        // Inactive until the schema is in place. The resolver already
        // filters on this column, so it costs nothing on the hot path
        // and is the only signal that keeps a half-provisioned tenant
        // from serving. `ProvisionStep::Activate` flips it.
        active: false,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    }
}

/// `CREATE SCHEMA` on the registry. An existing schema is refused, not
/// adopted: it may hold another app's data, and `purge-tenant` drops it (#2394).
/// A resumed run never gets here; its schema is already in place.
///
/// Schema mode is Postgres-only: `CREATE SCHEMA` and `SET search_path`
/// do not exist on SQLite or MySQL. There are two separate refusals:
/// the build has no `postgres` feature, or it does but these pools are
/// not Postgres, which only a runtime downcast can tell.
pub(crate) async fn provision_schema<DB: Database>(
    pools: &TenantPools<DB>,
    schema: &str,
) -> Result<(), TenancyError> {
    #[cfg(feature = "postgres")]
    {
        use crate::sql::Dialect as _;

        let pg_pools = (pools as &dyn std::any::Any)
            .downcast_ref::<TenantPools<sqlx::Postgres>>()
            .ok_or_else(|| {
                TenancyError::Validation(
                    "schema mode needs a Postgres registry — choose the database storage mode"
                        .into(),
                )
            })?;
        // Use the dialect's quoter, not a local copy: it doubles any
        // embedded `"`.
        let sql = format!("CREATE SCHEMA {}", crate::sql::Postgres.quote_ident(schema));
        match rustango::sql::sqlx::query(&sql)
            .execute(pg_pools.registry())
            .await
        {
            Ok(_) => Ok(()),
            // duplicate_schema
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P06") => {
                Err(TenancyError::Validation(format!(
                    "schema `{schema}` already exists — choose another schema name, or, if a \
                     failed create left it empty, run `DROP SCHEMA {}`",
                    crate::sql::Postgres.quote_ident(schema)
                )))
            }
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (pools, schema);
        Err(TenancyError::Validation(
            "schema mode is not available on this server — choose the database storage mode".into(),
        ))
    }
}

/// Drop the schema [`provision_schema`] just made when its `Org` row failed,
/// so a retry is not refused as taken. `RESTRICT`: an empty schema only.
pub(crate) async fn release_schema<DB: Database>(pools: &TenantPools<DB>, schema: &str) {
    #[cfg(feature = "postgres")]
    if let Some(pg_pools) =
        (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
    {
        use crate::sql::Dialect as _;
        let sql = format!(
            "DROP SCHEMA {} RESTRICT",
            crate::sql::Postgres.quote_ident(schema)
        );
        if let Err(e) = rustango::sql::sqlx::query(&sql)
            .execute(pg_pools.registry())
            .await
        {
            tracing::warn!(target: "rustango::tenancy::provision", schema, error = %e, "could not drop the new schema");
        }
    }
    #[cfg(not(feature = "postgres"))]
    let _ = (pools, schema);
}

/// Flip the tenant live.
pub(crate) async fn activate(registry: &crate::sql::Pool, org_id: i64) -> Result<(), TenancyError> {
    use crate::sql::UpdaterPool as _;
    let updated = Org::objects()
        .where_(Org::id.eq(org_id))
        .update()
        .set("active", true)
        .execute_pool(registry)
        .await?;
    if updated == 0 {
        return Err(TenancyError::Validation(format!(
            "activate: no row updated for org {org_id} — was it deleted mid-provision?"
        )));
    }
    Ok(())
}

/// Migrate the tenant that was just created, and only that one.
///
/// Uses [`tenant_migrate::migrate_one_tenant`], not the batch. The new
/// tenant is still inactive, so the batch would skip it; and the batch
/// would also migrate every other tenant, dragging an unrelated broken
/// chain into this run.
///
/// Never returns `Err`: the `Org` row already exists, and an error
/// return would hide that. A failure comes back as
/// [`MigrationsOutcome::Failed`].
async fn migrate_new_tenant<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    org: &Org,
    rep: &Reporter<'_>,
) -> MigrationsOutcome
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // Forward every migration event to the reporter, so a caller
    // watching a provisioning run sees the same per-migration detail
    // `manage migrate` prints (#1320). The reporter decides what to do
    // with them — the observer gets them now, the store when the lock
    // is released.
    let forward = move |event: tenant_migrate::TenantMigrationEvent| rep.migration(event);
    let forward: Option<&dyn tenant_migrate::TenantMigrationObserver> = Some(&forward);

    // One call, no backend branch: `migrate_one_tenant` owns the
    // schema-mode-is-PG-only dispatch that used to be duplicated here.
    match tenant_migrate::migrate_one_tenant(pools, org, dir, registry_url, forward).await {
        Ok(applied) => MigrationsOutcome::Applied(applied),
        Err(e) => MigrationsOutcome::Failed(e.to_string()),
    }
}

/// Provisioning for surfaces that have erased the backend type.
///
/// The operator console holds its pools as
/// `Arc<dyn TenantPoolInvalidator>` — deliberately, so `<DB>` does not
/// cascade through every handler — while [`provision_tenant`] is
/// generic over `DB`. This is the bridge, following the same
/// boxed-future shape [`super::TenantPoolInvalidator`] already uses.
///
/// It also closes over the registry URL and migrations directory, which
/// a request handler has no business carrying around.
///
/// Note what is **not** here: an observer. A caller watching a run
/// reads it back from [`super::provision_store`] instead, which is what
/// makes a console work when the pod serving the stream is not the pod
/// doing the work.
pub trait TenantProvisioner: Send + Sync {
    /// Stand up a tenant, recording the run.
    fn provision<'a>(
        &'a self,
        request: &'a ProvisionRequest,
        requested_by: Option<&'a str>,
        idempotency_key: Option<&'a str>,
    ) -> BoxFuture<'a, (super::provision_store::ProvisioningRun, ProvisionOutcome)>;

    /// Stand up a tenant into a run the caller already opened.
    ///
    /// The inbound webhook opens its run synchronously — so the
    /// response can carry an id, and so the `idempotency_key` unique
    /// constraint fires where an HTTP status can report it — and then
    /// hands the slow part to a task. Without this the task would open
    /// a *second* run for the same tenant.
    fn provision_in_run<'a>(
        &'a self,
        run_id: i64,
        request: &'a ProvisionRequest,
    ) -> BoxFuture<'a, ProvisionOutcome>;

    /// Migrate one tenant, or every active one when `slug` is `None`,
    /// recording into an already-open run.
    ///
    /// It lives here, not on its own trait, for the same reason as the
    /// rest: the console holds `Arc<dyn TenantProvisioner>` and has no
    /// `DB` to name.
    fn migrate_in_run<'a>(&'a self, run_id: i64, slug: Option<&'a str>) -> BoxFuture<'a, ()>;

    /// The registry's own connection URL, for deriving a tenant URL on
    /// the same server. **Never render it**: it carries credentials.
    /// Derive first, then redact for display.
    fn registry_url(&self) -> String;

    /// The registry pool, so a caller can read runs and events back.
    fn registry(&self) -> crate::sql::Pool;
}

/// The boxed-future shape an object-safe async method has to return.
///
/// Spelled once: written out inline it is four lines of angle brackets
/// per method, which buries what each one actually does.
pub type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, TenancyError>> + Send + 'a>>;

/// A [`TenantProvisioner`] over concrete pools.
pub struct Provisioner<DB: Database> {
    pools: std::sync::Arc<TenantPools<DB>>,
    registry_url: String,
    migrations_dir: std::path::PathBuf,
}

impl<DB: Database> Provisioner<DB> {
    pub fn new(
        pools: std::sync::Arc<TenantPools<DB>>,
        registry_url: impl Into<String>,
        migrations_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            pools,
            registry_url: registry_url.into(),
            migrations_dir: migrations_dir.into(),
        }
    }

    /// Type-erase for the console and anything else that does not want
    /// `<DB>` in its state.
    #[must_use]
    pub fn erased(self) -> std::sync::Arc<dyn TenantProvisioner>
    where
        crate::sql::Pool: From<sqlx::Pool<DB>>,
    {
        std::sync::Arc::new(self)
    }
}

impl<DB: Database> TenantProvisioner for Provisioner<DB>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    fn provision<'a>(
        &'a self,
        request: &'a ProvisionRequest,
        requested_by: Option<&'a str>,
        idempotency_key: Option<&'a str>,
    ) -> BoxFuture<'a, (super::provision_store::ProvisioningRun, ProvisionOutcome)> {
        Box::pin(async move {
            provision_tenant_recorded(
                self.pools.as_ref(),
                &self.registry_url,
                &self.migrations_dir,
                request,
                None,
                requested_by,
                idempotency_key,
            )
            .await
        })
    }

    fn provision_in_run<'a>(
        &'a self,
        run_id: i64,
        request: &'a ProvisionRequest,
    ) -> BoxFuture<'a, ProvisionOutcome> {
        Box::pin(async move {
            provision_tenant_in_run(
                self.pools.as_ref(),
                &self.registry_url,
                &self.migrations_dir,
                request,
                None,
                run_id,
            )
            .await
        })
    }

    fn migrate_in_run<'a>(&'a self, run_id: i64, slug: Option<&'a str>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            super::migrate_run::migrate_in_run(
                self.pools.as_ref(),
                &self.migrations_dir,
                &self.registry_url,
                run_id,
                slug,
            )
            .await
        })
    }

    fn registry_url(&self) -> String {
        self.registry_url.clone()
    }

    fn registry(&self) -> crate::sql::Pool {
        self.pools.registry_pool()
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    /// The exact input that created a live, unreachable tenant during
    /// QA: a space in the slug, accepted by the console, producing
    /// `host_pattern = "tennant 1.localhost"`.
    #[test]
    fn a_slug_with_a_space_is_refused() {
        let err = validate_slug("tennant 1").expect_err("a space is not a hostname character");
        assert!(err.contains("tennant 1"), "{err}");
        assert!(err.contains("hostname"), "should say why: {err}");
    }

    #[test]
    fn a_slug_is_restricted_to_what_is_legal_in_all_three_uses() {
        for bad in [
            "",          // empty
            "Acme",      // uppercase — not a hostname label
            "ac me",     // space
            "acme_corp", // underscore is legal in a DB name, not a hostname
            "acme.corp", // dot would make it two labels
            "acme;DROP", // punctuation
            "../etc",    // traversal
            "-acme",     // leading hyphen
            "acme-",     // trailing hyphen
        ] {
            assert!(
                validate_slug(bad).is_err(),
                "slug `{bad}` should have been refused"
            );
        }
        for good in ["acme", "acme-2", "a", "tenant-42"] {
            assert!(
                validate_slug(good).is_ok(),
                "slug `{good}` should be allowed"
            );
        }
    }

    /// The two schema names that were actually posted through the
    /// console during QA. Both were accepted, and both became real
    /// Postgres schemas behind live, active tenants.
    ///
    /// `quote_ident` did hold — no `pwned_a` table was created and
    /// `rustango_operators` was still there — so this is not a fix for
    /// an injection that worked. It is a fix for the schema name being
    /// unchecked, which is how an operator ends up with a tenant whose
    /// identifier they cannot type again.
    #[test]
    fn the_schema_names_that_got_through_qa_are_refused() {
        for injected in [
            r#"x"; CREATE TABLE public.pwned_a(i int); --"#,
            r#"y"; DROP TABLE public.rustango_operators; --"#,
        ] {
            let err =
                validate_schema_name(injected).expect_err("an SQL fragment is not a schema name");
            assert!(err.contains("only lowercase"), "{err}");
        }
    }

    #[test]
    fn a_schema_name_is_a_postgres_identifier() {
        for bad in [
            "",                   // empty
            "Acme",               // uppercase would be a *different* schema
            "ac me",              // space
            "acme;DROP",          // punctuation
            "acme.other",         // a dot is schema-qualification
            "-acme",              // leading hyphen
            "pg_toast",           // reserved
            "pg_anything",        // the whole `pg_` namespace is reserved
            "information_schema", // reserved
            "public",             // the registry's schema (#1868)
        ] {
            assert!(
                validate_schema_name(bad).is_err(),
                "schema name `{bad}` should have been refused"
            );
        }
        // Underscores are the difference from the slug rule: legal in
        // an identifier, illegal in a hostname label.
        for good in ["acme", "acme_corp", "tenant-42", "a", "2acme"] {
            assert!(
                validate_schema_name(good).is_ok(),
                "schema name `{good}` should be allowed"
            );
        }
        assert!(validate_schema_name(&"a".repeat(63)).is_ok());
        assert!(
            validate_schema_name(&"a".repeat(64)).is_err(),
            "Postgres would silently truncate it"
        );
    }

    /// The resolver lowercases the `Host` header and strips `:port`
    /// before comparing. Anything that cannot come out of that
    /// normalization can never match.
    #[test]
    fn a_host_pattern_that_could_never_match_is_refused() {
        let err = validate_host_pattern("acme.example.com:8080")
            .expect_err("the port is stripped before matching");
        assert!(
            err.contains("port field"),
            "should say where it goes: {err}"
        );

        let err = validate_host_pattern("*.example.com").expect_err("patterns are matched exactly");
        assert!(err.contains("wildcard"), "{err}");

        for bad in [
            "not a hostname",  // spaces
            "acme..com",       // empty label
            "acme.com.",       // trailing dot leaves an empty label
            "-acme.com",       // leading hyphen in a label
            "acme-.com",       // trailing hyphen in a label
            r#"acme";DROP--"#, // punctuation
        ] {
            assert!(
                validate_host_pattern(bad).is_err(),
                "host pattern `{bad}` should have been refused"
            );
        }
    }

    /// Uppercase is the one case that is normalized instead: the
    /// resolver lowercases the header anyway, so the operator's intent
    /// is not in doubt and refusing would be pedantry.
    #[test]
    fn an_uppercase_host_pattern_is_lowercased_not_refused() {
        assert_eq!(
            validate_host_pattern("Acme.Example.COM").as_deref(),
            Ok("acme.example.com")
        );
    }

    /// `PathPrefixResolver` looks up `"/<first segment>"`. Nothing else
    /// is ever the lookup key.
    #[test]
    fn a_path_prefix_must_be_one_leading_slash_segment() {
        for bad in [
            "acme",             // no leading slash
            "/",                // the apex resolves to no tenant
            "/acme/",           // trailing slash is a second, empty segment
            "/acme/dashboard",  // only the first segment is matched
            "../../etc/passwd", // the traversal string tried in QA
            "/..",              // a relative segment
            "/ac me",           // a space would arrive percent-encoded
        ] {
            assert!(
                validate_path_prefix(bad).is_err(),
                "path prefix `{bad}` should have been refused"
            );
        }
        for good in ["/acme", "/tenant-42", "/a_b", "/v1.0"] {
            assert!(
                validate_path_prefix(good).is_ok(),
                "path prefix `{good}` should be allowed"
            );
        }
    }

    /// `i32` is the column's type, not a port's range. Both of these
    /// parsed and were stored during QA.
    #[test]
    fn a_port_outside_the_tcp_range_is_refused() {
        assert!(validate_port(-1).is_err());
        assert!(validate_port(0).is_err());
        assert!(validate_port(999_999).is_err());
        assert!(validate_port(1).is_ok());
        assert!(validate_port(8080).is_ok());
        assert!(validate_port(65_535).is_ok());
    }

    /// The rules have to be reachable from the one place every caller
    /// goes through, or they are only the console's rules again.
    #[test]
    fn validate_fields_checks_the_slug_derived_schema_name_too() {
        // A slug legal as a hostname label is legal as a schema name,
        // so the default never trips its own rule.
        let mut req = ProvisionRequest::database("acme-2", "postgres://h/tenant");
        req.mode = StorageMode::Schema;
        req.database_url = None;
        assert!(validate_fields(&req).is_ok());

        // An explicit one is checked by the same rule.
        req.schema_name = Some(r#"x"; DROP TABLE t; --"#.into());
        assert!(validate_fields(&req).is_err());
    }

    #[test]
    fn validate_fields_hands_back_a_normalized_host_pattern() {
        let mut req = ProvisionRequest::database("acme", "postgres://h/tenant");
        req.host_pattern = Some("ACME.Example.com".into());
        let out = validate_fields(&req).expect("a legal hostname");
        assert_eq!(out.host_pattern.as_deref(), Some("acme.example.com"));
    }

    /// The default host pattern uses `[tenancy] apex_domain` too (#2225).
    #[test]
    fn the_default_host_pattern_reads_the_apex_setting() {
        assert!(
            std::env::var("RUSTANGO_APEX_DOMAIN").is_err(),
            "unset RUSTANGO_APEX_DOMAIN to run this test; env wins over the setting"
        );
        let _g = crate::tenancy::server::APEX_TEST_LOCK.blocking_lock();
        crate::tenancy::server::reset_apex_domain_setting();
        crate::tenancy::server::set_apex_domain_setting("apex.test");
        let req = ProvisionRequest::database("acme", "postgres://h/tenant");
        let out = validate_fields(&req);
        crate::tenancy::server::reset_apex_domain_setting();
        assert_eq!(
            out.expect("legal").host_pattern.as_deref(),
            Some("acme.apex.test")
        );
    }

    #[test]
    fn an_over_long_slug_is_refused_at_the_hostname_limit() {
        assert!(validate_slug(&"a".repeat(63)).is_ok());
        let err = validate_slug(&"a".repeat(64)).expect_err("too long for a hostname label");
        assert!(err.contains("63"), "{err}");
    }

    /// The critical QA finding: a tenant pointed at the registry's own
    /// database ran the tenant migration chain over the registry.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_tenant_url_naming_the_registry_database_is_refused() {
        let registry = "postgres://rustango:rustango@localhost:5432/orgdemo_dev";
        let err = refuse_registry_url(registry, registry).expect_err("same database");
        assert!(err.contains("registry's own database"), "{err}");
        // And it must not echo the password while saying so.
        assert!(!err.contains("rustango:rustango"), "password leaked: {err}");
    }

    /// Different credentials, same database, is still the same
    /// database — which is the case a naive string compare misses.
    #[cfg(feature = "postgres")]
    #[test]
    fn different_credentials_for_the_same_database_are_still_refused() {
        assert!(refuse_registry_url(
            "postgres://someone_else:other@localhost:5432/orgdemo_dev",
            "postgres://rustango:rustango@localhost:5432/orgdemo_dev",
        )
        .is_err());
    }

    /// And a query string does not make it a different database —
    /// sqlite's `?mode=rwc` in particular.
    #[cfg(feature = "sqlite")]
    #[test]
    fn a_query_string_does_not_disguise_the_same_database() {
        assert!(refuse_registry_url(
            "sqlite:///var/app/reg.db?mode=rwc",
            "sqlite:///var/app/reg.db",
        )
        .is_err());
    }

    #[test]
    fn a_genuinely_separate_database_is_allowed() {
        let registry = "postgres://rustango:rustango@localhost:5432/orgdemo_dev";
        for ok in [
            "postgres://rustango:rustango@localhost:5432/acme_tenant", // other db
            "postgres://rustango:rustango@otherhost:5432/orgdemo_dev", // other host
            "postgres://rustango:rustango@localhost:5433/orgdemo_dev", // other port
        ] {
            assert!(
                refuse_registry_url(ok, registry).is_ok(),
                "`{ok}` is a different database and should be allowed"
            );
        }
    }

    /// #1332 asked for the derivation to be exercised on every dialect,
    /// because "same server, another database" is a different shape in
    /// each: a path segment on a server, a sibling file on sqlite.
    #[test]
    fn a_server_url_keeps_everything_but_the_database() {
        assert_eq!(
            tenant_url_on_registry_server(
                "postgres://app:pw@db.internal:5432/orgdemo_dev",
                "tenant_acme"
            )
            .as_deref(),
            Some("postgres://app:pw@db.internal:5432/tenant_acme")
        );
        assert_eq!(
            tenant_url_on_registry_server("mysql://app:pw@db.internal:3306/orgdemo_dev", "t_acme")
                .as_deref(),
            Some("mysql://app:pw@db.internal:3306/t_acme")
        );
    }

    /// #1932: the query is split off before the path, so a `/` inside it
    /// is not the database segment and TLS options carry over.
    #[test]
    fn a_server_url_keeps_its_query_string() {
        for (registry, want) in [
            (
                "postgres://app@db:5432/reg?sslmode=verify-full&sslrootcert=/etc/ssl/ca.pem",
                "postgres://app@db:5432/tenant_acme?sslmode=verify-full&sslrootcert=/etc/ssl/ca.pem",
            ),
            (
                "postgres://app@db:5432/reg?sslmode=require",
                "postgres://app@db:5432/tenant_acme?sslmode=require",
            ),
            (
                "mysql://app@db:3306/reg?ssl-mode=REQUIRED",
                "mysql://app@db:3306/tenant_acme?ssl-mode=REQUIRED",
            ),
        ] {
            assert_eq!(
                tenant_url_on_registry_server(registry, "tenant_acme").as_deref(),
                Some(want),
                "registry `{registry}`"
            );
        }
        assert!(tenant_url_on_registry_server("postgres://db?sslrootcert=/ca.pem", "t").is_none());
    }

    /// sqlx reads `dbname`/`password`/`host` from the query and lets it
    /// win, so a kept non-TLS key would aim tenant migrations elsewhere.
    #[test]
    fn a_server_url_drops_every_non_tls_query_key() {
        for (registry, want) in [
            (
                "postgres://app@db:5432/reg?dbname=reg",
                "postgres://app@db:5432/tenant_acme",
            ),
            (
                "postgres://app@db:5432/reg?password=s3cret&sslmode=require",
                "postgres://app@db:5432/tenant_acme?sslmode=require",
            ),
            (
                "mysql://app@db:3306/reg?ssl-ca=/ca.pem&socket=/tmp/x",
                "mysql://app@db:3306/tenant_acme?ssl-ca=/ca.pem",
            ),
        ] {
            let derived = tenant_url_on_registry_server(registry, "tenant_acme");
            assert_eq!(derived.as_deref(), Some(want), "registry `{registry}`");
            assert!(refuse_registry_url(&derived.unwrap(), registry).is_ok());
        }
    }

    /// A tenant URL whose `dbname=` names the registry is the registry.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_dbname_query_naming_the_registry_is_refused() {
        let registry = "postgres://app:pw@db:5432/reg";
        for tenant in [
            "postgres://other@db:5432/tenant_acme?dbname=reg",
            "postgres://db:5432?dbname=reg",
            "postgresql://db:5432/x?dbname=t&dbname=reg",
        ] {
            assert!(
                refuse_registry_url(tenant, registry).is_err(),
                "`{tenant}` names the registry"
            );
        }
        assert!(refuse_registry_url("postgres://db:5432/reg?dbname=t", registry).is_ok());
    }

    /// sqlite has no server, so the sibling is a file in the registry
    /// file's own directory — including the `./` form the scaffolder
    /// writes into every generated `.env.example`.
    #[test]
    fn a_sqlite_url_derives_a_sibling_file() {
        for (registry, want) in [
            (
                "sqlite://./demo_dev.db?mode=rwc",
                "sqlite://./tenant_acme.db?mode=rwc",
            ),
            (
                "sqlite:///var/app/reg.db",
                "sqlite:///var/app/tenant_acme.db?mode=rwc",
            ),
            // A bare filename is relative to the working directory.
            // This derived nothing at all before, so a registry
            // configured that way fell back to "type a URL yourself".
            ("sqlite://reg.db", "sqlite://./tenant_acme.db?mode=rwc"),
        ] {
            assert_eq!(
                tenant_url_on_registry_server(registry, "tenant_acme").as_deref(),
                Some(want),
                "registry `{registry}`"
            );
        }
    }

    /// An operator naming the database `acme.db` means the file
    /// `acme.db`, not `acme.db.db`.
    #[test]
    fn a_sqlite_database_name_is_not_given_two_extensions() {
        for named in ["acme.db", "acme.DB"] {
            assert_eq!(
                tenant_url_on_registry_server("sqlite://./reg.db", named).as_deref(),
                Some(format!("sqlite://./{named}?mode=rwc")).as_deref(),
                "database name `{named}`"
            );
        }
        // A dot that is not the extension still gets one.
        assert_eq!(
            tenant_url_on_registry_server("sqlite://./reg.db", "acme.v2").as_deref(),
            Some("sqlite://./acme.v2.db?mode=rwc")
        );
    }

    /// Shapes with nowhere to put a tenant. The caller renders the
    /// "supply a full URL instead" escape hatch for these rather than
    /// guessing.
    #[test]
    fn a_url_with_no_database_to_replace_derives_nothing() {
        for undrivable in [
            "sqlite://:memory:",    // no directory to be a sibling of
            "sqlite://",            // no path at all
            "postgres://localhost", // no database segment
            "not a url",            // no scheme
            "postgres:///orgdemo",  // no authority
        ] {
            assert!(
                tenant_url_on_registry_server(undrivable, "tenant_acme").is_none(),
                "`{undrivable}` should not derive a URL"
            );
        }
    }

    /// The derivation and the registry-collision guard have to agree:
    /// a derived URL must never be the one it was derived from.
    #[test]
    fn a_derived_url_is_never_the_registry_itself() {
        for registry in [
            "postgres://app:pw@db.internal:5432/orgdemo_dev",
            "sqlite://./demo_dev.db?mode=rwc",
        ] {
            let derived =
                tenant_url_on_registry_server(registry, "tenant_acme").expect("derivable registry");
            assert!(
                refuse_registry_url(&derived, registry).is_ok(),
                "derived `{derived}` collided with registry `{registry}`"
            );
        }
    }
}

/// #2320: the registry refusal reads a tenant URL the way the pool did.
/// Every URL names its port or shares the pool's default, so `PG*` env can't skew it.
#[cfg(test)]
mod registry_endpoint_tests {
    use super::*;

    #[cfg(feature = "postgres")]
    fn pg(url: &str) -> crate::sql::Pool {
        crate::sql::Pool::Postgres(sqlx::PgPool::connect_lazy(url).expect("lazy"))
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn an_empty_host_is_the_same_default_host() {
        let pool = pg("postgres:///reg?user=app");
        assert!(refuse_registry_pool("postgres:///reg", &pool).is_err());
        assert!(refuse_registry_pool("postgres:///reg?user=other&password=pw", &pool).is_err());
        assert!(refuse_registry_pool("postgres:///tenant", &pool).is_ok());
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn query_host_port_and_socket_overrides_are_read() {
        let pool = pg("postgres://app@db.internal:5432/reg");
        for same in [
            "postgres://elsewhere:5432/x?host=db.internal&dbname=reg",
            "postgres://db.internal:6000/reg?port=5432",
            "postgres://DB.internal:5432/reg",
        ] {
            assert!(refuse_registry_pool(same, &pool).is_err(), "{same}");
        }
        for other in [
            "postgres://db.internal:5433/reg",
            "postgres://db.internal:5432/tenant",
            "postgres://db.internal:5432/reg?host=elsewhere",
            "mysql://db.internal:3306/reg",
        ] {
            assert!(refuse_registry_pool(other, &pool).is_ok(), "{other}");
        }
        // A PG tenant pool ignores the scheme, so these connect to the registry.
        for same in ["mysql://db.internal:5432/reg", "x://db.internal:5432/reg"] {
            assert!(refuse_registry_pool(same, &pool).is_err(), "{same}");
        }
        let socket = pg("postgres://app@%2Fcloudsql%2Fp:5432/reg");
        assert!(refuse_registry_pool("postgres://x:5432/reg?host=/cloudsql/p", &socket).is_err());
    }

    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn mariadb_is_mysql() {
        let pool = crate::sql::Pool::Mysql(
            sqlx::MySqlPool::connect_lazy("mysql://app:pw@db:3306/reg").expect("lazy"),
        );
        assert!(refuse_registry_pool("mariadb://x@db:3306/reg", &pool).is_err());
        assert!(refuse_registry_pool("mysql://x@db/reg", &pool).is_err());
        assert!(refuse_registry_pool("mysql://x@db:3306/tenant", &pool).is_ok());
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn an_unreadable_url_is_refused() {
        let pool = pg("postgres://app@db.internal:5432/reg");
        for bad in [
            "postgres://db.internal:notaport/reg",
            "postgres://db.internal:5432/reg?port=x",
        ] {
            let err = refuse_registry_pool(bad, &pool).expect_err(bad);
            assert!(err.contains("cannot read"), "{err}");
        }
        assert!(refuse_registry_url("postgres://h:x/reg", "postgres://h:5432/reg").is_err());
    }

    /// Org edits have no secrets resolver, so a reference passes unchecked.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn a_secret_reference_is_not_refused() {
        let pool = pg("postgres://app@db.internal:5432/reg");
        for reference in [
            "acme-db",
            "env://TENANT_DB",
            "aws-sm://arn:aws:secretsmanager:eu:1:secret:db",
        ] {
            assert!(
                refuse_registry_pool(reference, &pool).is_ok(),
                "{reference}"
            );
        }
    }

    /// A default or `PGHOST` socket dir sits in `host`; `?host=/dir` in `socket`.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn a_socket_dir_matches_in_either_spelling() {
        let opts = sqlx::postgres::PgConnectOptions::new_without_pgpass()
            .host("/tmp")
            .port(5432)
            .database("reg");
        let pool = crate::sql::Pool::Postgres(sqlx::PgPool::connect_lazy_with(opts));
        assert!(refuse_registry_pool("postgres://x:5432/reg?host=/tmp", &pool).is_err());
        assert!(refuse_registry_pool("postgres://x:5432/reg?host=/tmp/", &pool).is_err());
        assert!(refuse_registry_pool("postgres://x:5432/t?host=/tmp", &pool).is_ok());
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn a_trailing_dot_or_brackets_name_the_same_host() {
        let pool = pg("postgres://app@db.internal:5432/reg");
        assert!(refuse_registry_pool("postgres://db.internal.:5432/reg", &pool).is_err());
        let v6 = pg("postgres://app@[::1]:5432/reg");
        assert!(refuse_registry_pool("postgres://x:5432/reg?host=::1", &v6).is_err());
    }

    /// Unix only: a Windows canonical path starts `\\?\`, which a URL reads as a query.
    #[cfg(all(feature = "sqlite", unix))]
    #[tokio::test]
    async fn a_relative_sqlite_path_is_the_same_file() {
        let dir = tempfile::tempdir_in(".").expect("tempdir");
        let file = dir.path().join("reg.db");
        std::fs::write(&file, b"").unwrap();
        let abs = std::fs::canonicalize(&file).unwrap();
        let pool = crate::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy(&format!("sqlite://{}", abs.display())).expect("lazy"),
        );
        let cwd = std::env::current_dir().unwrap();
        let rel = std::path::Path::new(".").join(abs.strip_prefix(&cwd).unwrap());
        let rel = rel.display();
        for same in [format!("sqlite:{rel}"), format!("sqlite://{rel}?mode=rwc")] {
            assert!(refuse_registry_pool(&same, &pool).is_err(), "{same}");
            assert!(refuse_registry_url(&same, &format!("sqlite://{}", abs.display())).is_err());
        }
        let abs_s = abs.display();
        for uri in [
            format!("sqlite:file:{rel}"),
            format!("sqlite:file:{abs_s}?mode=rwc"),
            format!("sqlite://file://{abs_s}"),
        ] {
            assert!(refuse_registry_pool(&uri, &pool).is_err(), "{uri}");
        }
        let sibling = format!("sqlite:{}", dir.path().join("t.db").display());
        assert!(refuse_registry_pool(&sibling, &pool).is_ok());
        // The tenant pool is SQLite too, so a bare path or a foreign scheme opens the file.
        let parent = abs.parent().unwrap().display();
        for same in [
            format!("{abs_s}"),
            format!("{rel}"),
            format!("{parent}/nope/../reg.db"),
            format!("sqlite:{parent}/nope/./../reg.db"),
        ] {
            assert!(refuse_registry_pool(&same, &pool).is_err(), "{same}");
        }
    }

    /// SQLite pops `..` by text, then follows a symlink in any later component.
    #[cfg(all(feature = "sqlite", unix))]
    #[tokio::test]
    async fn a_symlink_after_a_missing_dir_is_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("reg.db"), b"").unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let pool = crate::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy(&format!("sqlite://{}", real.join("reg.db").display()))
                .expect("lazy"),
        );
        let d = dir.path().display();
        for same in [
            format!("sqlite:{d}/nope/../link/reg.db"),
            format!("sqlite:{d}/link/nope/../reg.db"),
        ] {
            assert!(refuse_registry_pool(&same, &pool).is_err(), "{same}");
        }
        let other = format!("sqlite:{d}/nope/../link/t.db");
        assert!(refuse_registry_pool(&other, &pool).is_ok());
    }
}
