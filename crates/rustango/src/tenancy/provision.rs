//! Standing up a tenant: the steps, in order, with nothing about a
//! terminal in them.
//!
//! These steps used to live inside the `create-tenant` verb, welded to a
//! CLI by three things — a `pub(super)` visibility, an `args: &[String]`
//! it parsed itself, and a `W: Write` it reported through. Anything that
//! is not a terminal (an HTTP handler, a webhook, a job) would have had
//! to fake an `argv` and hand it a `Vec<u8>`.
//!
//! So the sequence moves here and the verb keeps what is genuinely a
//! CLI's job: turning `argv` into a [`ProvisionRequest`], and rendering
//! [`ProvisionEvent`]s as lines of text.
//!
//! ## The steps
//!
//! 1. [`Validate`](ProvisionStep::Validate) — slug, mode and backend
//!    agree with each other and with this build.
//! 2. [`CheckConnection`](ProvisionStep::CheckConnection) — reach the
//!    tenant's database and prove this role can create tables in it,
//!    **before** anything is written. See [`super::preflight`].
//!    Schema-mode skips it: those tenants live in the registry's own
//!    database, which is already connected.
//! 3. [`ProvisionStorage`](ProvisionStep::ProvisionStorage) —
//!    `CREATE SCHEMA` for schema-mode. Deliberately before the row
//!    lands, so a failed `INSERT` leaves no orphan schema.
//! 4. [`RegisterOrg`](ProvisionStep::RegisterOrg) — the `rustango_orgs`
//!    row, written **inactive**.
//! 5. [`Migrate`](ProvisionStep::Migrate) — the tenant's own schema,
//!    through [`tenant_migrate::migrate_one_tenant`]: this tenant, not
//!    the whole active batch.
//! 6. [`Activate`](ProvisionStep::Activate) — flip `active`, after
//!    which the tenant resolves.
//!
//! ## Inactive until ready, and what a failed run leaves
//!
//! The row goes in inactive and is activated last. That is the whole
//! answer to "what does a half-provisioned tenant do": nothing, because
//! the resolver filters on `active` and it is not set yet. There is no
//! window in which a tenant resolves to a database with no schema.
//!
//! A run that fails at the migrate step therefore leaves an **inactive
//! `Org` row plus whatever schema landed**. The row stays on purpose:
//! an operator needs to see what was half-made, and rolling it back is
//! not always possible once a schema or a database exists. Nothing
//! routes to it in the meantime.
//!
//! This overloads `active`, which until now meant "suspended customer"
//! and now also means "never finished provisioning". The two are
//! identical to the resolver — do not serve — and the distinction that
//! does matter is answerable from
//! [`super::provision_store`], which is where the detail belongs. A
//! dedicated `provisioning_state` column would be tidier and would cost
//! a core-model change rippling through every hand-written test schema,
//! for a distinction only the console needs.

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

/// Everything needed to stand up one tenant.
///
/// The CLI's own argument struct, made public and given one rename:
/// `no_migrate` became [`run_migrations`](Self::run_migrations). A
/// negative flag is right for a command line, where the default is
/// "yes" and you opt out; it is wrong for a struct field, where every
/// reader has to hold an extra negation.
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
    /// Flip `active`, after which the tenant resolves. Last on purpose:
    /// it is what closes the window where a half-provisioned tenant
    /// serves requests against a database with no schema.
    Activate,
}

/// How a step went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Started,
    Ok,
    /// Nothing to do, and why — a database-mode tenant has no schema to
    /// create, a caller asked for no migrations, a step is not built
    /// yet. Distinct from `Ok` so a console can grey it out rather than
    /// claim work that never happened.
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
    /// A migration-level event from the tenant's own run — forwarded
    /// straight through, so a caller watching provisioning sees the
    /// same per-migration detail `manage migrate` prints.
    Migration(tenant_migrate::TenantMigrationEvent),
}

/// Receives [`ProvisionEvent`]s as a run progresses.
///
/// Implemented for any `Fn(ProvisionEvent)`, so a closure works
/// directly. **Must not block**: the migration events forwarded through
/// it are emitted with the tenant's migrate lock held. See
/// [`crate::migrate::progress`].
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
    /// The batch ran, but its report had no row for this tenant —
    /// usually a migration directory with nothing tenant-scoped in it.
    NotMatched,
    Applied(Vec<Migration>),
    /// The tenant's chain failed. **Not** returned as an `Err`: the row
    /// exists and the caller needs to know that, which an error return
    /// would hide.
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
/// ## Why migration events are buffered and step transitions are not
///
/// Step transitions happen between steps, with no lock held, so they
/// are written as they occur — which is what makes a run readable from
/// a second pod while it is still going.
///
/// Migration events are different: they arrive **synchronously from
/// inside the migrate lock** (see [`crate::migrate::progress`]), where
/// an `await` on a registry write would extend a lock every other
/// migrating pod is queued behind. So they are buffered and flushed
/// once the migrate step ends.
///
/// The cost is that a watcher sees `Migrate: started`, then a pause,
/// then the whole per-migration log at once — rather than line by line.
/// The alternative is a bounded channel and a drain task, which trades
/// that for dropped events when the channel fills, and a `seq` with
/// holes in it is no use as a replay log. Worth revisiting when the
/// console (#1322) shows whether the pause actually matters.
pub(super) struct Reporter<'a> {
    observer: Option<&'a dyn ProvisionObserver>,
    store: Option<RunStore<'a>>,
}

struct RunStore<'a> {
    registry: &'a crate::sql::Pool,
    run_id: i64,
    /// Next `seq`. Dense and per-run, because `Last-Event-ID` counts
    /// from it.
    seq: std::sync::atomic::AtomicI64,
    buffered: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl<'a> Reporter<'a> {
    pub(super) fn new(observer: Option<&'a dyn ProvisionObserver>) -> Self {
        Self {
            observer,
            store: None,
        }
    }

    /// Also persist to `run_id` in the registry.
    pub(super) fn persisting(mut self, registry: &'a crate::sql::Pool, run_id: i64) -> Self {
        self.store = Some(RunStore {
            registry,
            run_id,
            seq: std::sync::atomic::AtomicI64::new(1),
            buffered: std::sync::Mutex::new(Vec::new()),
        });
        self
    }

    fn notify(&self, event: ProvisionEvent) {
        if let Some(observer) = self.observer {
            observer.on_event(event);
        }
    }

    /// Write one row now. Failures are logged, never propagated: the
    /// record of a provisioning run must not be what fails it.
    async fn write(&self, step: &str, status: &str, message: &str) {
        let Some(store) = &self.store else {
            return;
        };
        let seq = store.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = super::provision_store::append_event(
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
    fn buffer(&self, step: &str, status: &str, message: &str) {
        let Some(store) = &self.store else {
            return;
        };
        if let Ok(mut buf) = store.buffered.lock() {
            buf.push((step.to_owned(), status.to_owned(), message.to_owned()));
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
            self.write(&step, &status, &message).await;
        }
    }
}

impl Reporter<'_> {
    /// Announce a step transition — to the observer, and to the store.
    async fn step(&self, step: ProvisionStep, status: StepStatus) {
        self.write(step.as_str(), status.as_str(), status.detail())
            .await;
        self.notify(ProvisionEvent::Step { step, status });
    }

    /// Report a step's failure, then hand back the error.
    ///
    /// Every failure path goes through this rather than a bare `?`, so
    /// a watcher never sees a step stuck on `Started` with no
    /// explanation — which is the state the whole event stream exists
    /// to prevent.
    async fn fail<T>(&self, at: ProvisionStep, e: TenancyError) -> Result<T, TenancyError> {
        self.step(at, StepStatus::Failed(e.to_string())).await;
        Err(e)
    }

    /// A migration event from the tenant's own run. Buffered rather
    /// than written — see the type docs.
    fn migration(&self, event: tenant_migrate::TenantMigrationEvent) {
        self.buffer("migration", "info", &render_migration(&event));
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

/// One line describing a migration event, for the stored log.
fn render_migration(event: &tenant_migrate::TenantMigrationEvent) -> String {
    use crate::migrate::{MigrationEvent as M, Outcome};
    use tenant_migrate::{Chain, TenantMigrationEvent as E};

    let chain_tag = |c: Chain| match c {
        Chain::System => "system",
        Chain::Project => "app",
    };
    match event {
        E::Planned { tenants } => format!("migrating {tenants} tenant(s)"),
        E::TenantStarted { slug, .. } => format!("tenant {slug}"),
        E::TenantFinished {
            slug,
            applied,
            error,
            ..
        } => match error {
            Some(e) => format!("tenant {slug} failed: {e}"),
            None => format!("tenant {slug}: {applied} migration(s)"),
        },
        E::Migration { chain, event, .. } => match event {
            M::Planned { total } => format!("{}: {total} pending", chain_tag(*chain)),
            M::Started { name, .. } => format!("{}/{name} started", chain_tag(*chain)),
            M::Finished {
                name,
                outcome,
                elapsed,
                ..
            } => {
                let verb = match outcome {
                    Outcome::Ran => "applied",
                    Outcome::RanPartial { .. } => "applied (partial)",
                    Outcome::Faked => "faked",
                };
                format!(
                    "{verb} {}/{name} ({:.1}s)",
                    chain_tag(*chain),
                    elapsed.as_secs_f64()
                )
            }
            M::Failed { name, error, .. } => {
                format!("{}/{name} failed: {error}", chain_tag(*chain))
            }
        },
    }
}

/// Stand up a tenant: validate, provision its storage, register it, and
/// migrate it.
///
/// The CLI verb is a thin wrapper over this. So is anything else that
/// needs to create a tenant.
///
/// # Errors
/// Returns `Err` if the slug is taken, the request is internally
/// inconsistent (database-mode without a URL, schema-mode on a non-PG
/// build), the schema could not be created, or the row could not be
/// inserted. A **migration** failure is not an error — the tenant
/// exists at that point, so it comes back in
/// [`ProvisionOutcome::migrations`] as
/// [`MigrationsOutcome::Failed`].
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
/// The durable half of the same call: step transitions are written as
/// they happen, so a run started on one pod is readable — and
/// replayable from the beginning — on another.
///
/// # Errors
/// As [`provision_tenant`]. A failure to *record* the run is logged and
/// never propagated: the bookkeeping must not be what fails a tenant
/// creation.
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

    let outcome = provision_tenant_in_run(pools, registry_url, dir, request, observer, run_id)
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
/// The inbound webhook needs this: it opens the run **synchronously**
/// so the caller gets an id back and so the `idempotency_key` unique
/// constraint fires where a response can carry it, then hands the slow
/// part to a task. Without this entry point that task would open a
/// *second* run for the same tenant.
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
    use super::provision_store::{self as store, RunState};

    let registry = pools.registry_pool();
    let rep = Reporter::new(observer).persisting(&registry, run_id);
    let result = provision_reported(pools, registry_url, dir, request, &rep).await;

    // Whatever happened, close the run — including on the error paths,
    // where a run left `running` forever is exactly the ambiguity this
    // table exists to remove.
    let (state, error) = match &result {
        Ok(outcome) => match &outcome.migrations {
            MigrationsOutcome::Failed(e) => (RunState::Failed, Some(e.clone())),
            _ => (RunState::Succeeded, None),
        },
        Err(e) => (RunState::Failed, Some(e.to_string())),
    };
    if let Ok(outcome) = &result {
        if let Err(e) = store::attach_org(&registry, run_id, outcome.org_id).await {
            tracing::warn!(target: "rustango::tenancy::provision", error = %e, "could not attach org id to run");
        }
    }
    if let Err(e) = store::finish_run(&registry, run_id, state, error.as_deref()).await {
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
    rep.step(ProvisionStep::Validate, StepStatus::Started).await;
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
    if !existing.is_empty() {
        return rep
            .fail(
                ProvisionStep::Validate,
                TenancyError::Validation(format!("tenant slug `{}` already exists", request.slug)),
            )
            .await;
    }

    // Every free-text field, not just the slug. Returns the request
    // back with `host_pattern` normalized — see `validate_fields`.
    let normalized = match validate_fields(request) {
        Ok(r) => r,
        Err(msg) => {
            return rep
                .fail(ProvisionStep::Validate, TenancyError::Validation(msg))
                .await;
        }
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

    if let Some(url) = &request.database_url {
        if let Err(msg) = refuse_registry_url(url, registry_url) {
            return rep
                .fail(ProvisionStep::Validate, TenancyError::Validation(msg))
                .await;
        }
    }
    rep.step(ProvisionStep::Validate, StepStatus::Ok).await;

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
    // orphan schema behind. Idempotent via `IF NOT EXISTS`.
    provision_storage(pools, request, schema_name.as_deref(), rep).await?;

    // ---- 4. Register the org ----
    rep.step(ProvisionStep::RegisterOrg, StepStatus::Started)
        .await;
    let mut org = new_org_row(request, schema_name);
    if let Err(e) = org.insert_pool(&registry).await {
        return rep.fail(ProvisionStep::RegisterOrg, e.into()).await;
    }
    // This pod sees the new tenant immediately; others converge on the
    // registry fingerprint (see `resolver::sync_org_generation`).
    super::invalidate_org_cache();
    let org_id = org.id.get().copied().unwrap_or_default();
    rep.step(ProvisionStep::RegisterOrg, StepStatus::Ok).await;
    rep.notify(ProvisionEvent::Registered { org_id });

    // ---- 5. Migrate ----
    let migrations = if request.run_migrations {
        rep.step(ProvisionStep::Migrate, StepStatus::Started).await;
        let outcome = migrate_new_tenant(pools, registry_url, dir, &org, rep).await;
        // The migration events arrived synchronously while the migrate
        // lock was held, so they were buffered. The lock is released by
        // now — write them before reporting the step's own outcome, so
        // the stored log reads in the order things happened.
        rep.flush().await;
        match &outcome {
            MigrationsOutcome::Failed(e) => {
                rep.step(ProvisionStep::Migrate, StepStatus::Failed(e.clone()))
                    .await;
            }
            _ => rep.step(ProvisionStep::Migrate, StepStatus::Ok).await,
        }
        outcome
    } else {
        rep.step(
            ProvisionStep::Migrate,
            StepStatus::Skipped("caller asked for no migrations".into()),
        )
        .await;
        MigrationsOutcome::Skipped
    };

    // ---- 6. Activate ----
    //
    // The row went in inactive (see `new_org_row`), so up to this point
    // the tenant does not resolve. Flipping it last is what closes the
    // window where a half-provisioned tenant serves requests against a
    // database with no schema in it.
    //
    // A migration failure leaves it inactive, deliberately. The row
    // stays — an operator needs to see what was half-made, and rolling
    // it back is not always possible anyway once a schema or database
    // exists — but nothing routes to it.
    if matches!(migrations, MigrationsOutcome::Failed(_)) {
        rep.step(
            ProvisionStep::Activate,
            StepStatus::Skipped("migrations failed; the tenant stays inactive".into()),
        )
        .await;
    } else {
        rep.step(ProvisionStep::Activate, StepStatus::Started).await;
        if let Err(e) = activate(&registry, org_id).await {
            return rep.fail(ProvisionStep::Activate, e).await;
        }
        super::invalidate_org_cache();
        rep.step(ProvisionStep::Activate, StepStatus::Ok).await;
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
            StepStatus::Skipped("schema-mode tenants share the registry's database".into()),
        )
        .await;
        return Ok(());
    };

    rep.step(ProvisionStep::CheckConnection, StepStatus::Started)
        .await;
    match preflight::check(url, &request.preflight).await {
        Ok(_) => {
            rep.step(ProvisionStep::CheckConnection, StepStatus::Ok)
                .await;
            Ok(())
        }
        // `Validation`, not a driver error: nothing is broken in
        // rustango, the URL the caller supplied is wrong, and the
        // diagnosis already says what to change.
        Err(d) => {
            rep.fail(
                ProvisionStep::CheckConnection,
                TenancyError::Validation(d.to_string()),
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
    request: &ProvisionRequest,
    schema_name: Option<&str>,
    rep: &Reporter<'_>,
) -> Result<(), TenancyError> {
    if request.mode != StorageMode::Schema {
        rep.step(
            ProvisionStep::ProvisionStorage,
            StepStatus::Skipped("database-mode tenants bring their own database".into()),
        )
        .await;
        return Ok(());
    }

    rep.step(ProvisionStep::ProvisionStorage, StepStatus::Started)
        .await;
    let schema = schema_name.unwrap_or(&request.slug);
    if let Err(e) = provision_schema(pools, schema).await {
        return rep.fail(ProvisionStep::ProvisionStorage, e).await;
    }
    rep.step(ProvisionStep::ProvisionStorage, StepStatus::Ok)
        .await;
    Ok(())
}

/// A slug becomes three things, and has to be legal in all of them.
///
/// It is a **database name**, a **schema name**, and a **hostname
/// label** (`<slug>.<apex>`). The last is the strictest: RFC 1123
/// allows only lowercase letters, digits and hyphens, not leading or
/// trailing.
///
/// This lived only in the inbound webhook, so the console — the path a
/// human uses — was the laxer of the two. `tennant 1`, with a space,
/// produced a **live tenant** whose `host_pattern` was
/// `tennant 1.localhost`: a hostname that cannot resolve, so the
/// tenant could never be reached, discovered only by someone
/// wondering why their new customer 404s.
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

/// Check every operator-supplied field, and hand back the request with
/// the one field that is normalized rather than refused.
///
/// One function, called from the `Validate` step, so the console, the
/// webhook and `manage create-tenant` cannot disagree about what a
/// legal tenant is. They did: the slug rule lived in the webhook alone
/// until a space in a console slug produced an unreachable tenant, and
/// these four fields had no rule anywhere at all.
///
/// The shared thread is that all four feed a matcher or an identifier
/// that is *exact*. A value that cannot be produced by the thing it is
/// compared against is not a configuration choice with an unusual
/// consequence — it is a tenant that is registered, active, and
/// unreachable, discovered by whoever eventually wonders why the new
/// customer 404s.
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
    if let Some(pattern) = &request.host_pattern {
        out.host_pattern = Some(validate_host_pattern(pattern)?);
    }
    Ok(out)
}

/// The schema a schema-mode tenant is created in.
///
/// This reaches `CREATE SCHEMA` and `SET search_path` as an
/// identifier. [`crate::sql::Dialect::quote_ident`] quotes it, so a
/// name carrying a quote and a semicolon does **not** execute — that
/// held when it was tried. What did not hold is everything after:
/// nothing rejected the name, so
/// `x"; CREATE TABLE public.pwned(i int); --` became a real schema and
/// a live, active tenant. An identifier nobody can type again without
/// quoting is not a tenant anybody can operate.
///
/// The rule is the slug's, plus underscores: a schema name is not a
/// hostname label, so `_` — which Postgres and every naming convention
/// allow — has no reason to be refused here.
fn validate_schema_name(name: &str) -> Result<(), String> {
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
    // Not "must start with a letter": the *default* schema name is the
    // slug, and a slug may start with a digit. Only the hyphen is
    // refused, being the one leading character that reads as a flag.
    if name.starts_with('-') {
        return Err(format!("schema name `{name}` may not start with a hyphen"));
    }
    // `pg_*` is reserved for system schemas; `CREATE SCHEMA pg_x` is
    // refused by the server with a message about the reservation, which
    // would surface here as a failed run rather than a validation error.
    if name.starts_with("pg_") || name == "information_schema" {
        return Err(format!(
            "schema name `{name}` is reserved by Postgres — choose another"
        ));
    }
    Ok(())
}

/// The `Host` header this tenant answers to.
///
/// Matched **exactly**, against a header the resolver has already
/// lowercased and stripped of its `:port` (see
/// `resolver::host_from_parts`). Three things therefore produce a
/// tenant that is registered, active, and unreachable forever:
/// uppercase, a `:port` suffix, and a wildcard — none of which can ever
/// equal the normalized header.
///
/// Uppercase is normalized rather than refused, because lowercasing it
/// is exactly what the resolver does and the operator's intent is not
/// in doubt. The other two are refused: they mean the operator wants
/// something this matcher does not do, and quietly storing a value that
/// cannot match would be the worse answer.
fn validate_host_pattern(pattern: &str) -> Result<String, String> {
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
fn validate_path_prefix(prefix: &str) -> Result<(), String> {
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
fn validate_port(port: i32) -> Result<(), String> {
    if !(1..=65535).contains(&port) {
        return Err(format!(
            "port {port} is outside the 1–65535 a TCP port can be"
        ));
    }
    Ok(())
}

/// Refuse a tenant URL that points at the registry's own database.
///
/// Nothing stopped this, and the consequence is not cosmetic:
/// provisioning ran the **tenant** migration chain into the
/// **registry**, creating `rustango_users`, `rustango_admin_users`,
/// the media tables and the project's own models there — and writing
/// `0001_initial` into the registry's project ledger, so a later
/// legitimate migration run reads a ledger that lies about what has
/// been applied. A project whose tenant migrations contain any
/// destructive operation would have had it run against the registry.
///
/// Compared on endpoint identity — scheme, host, port, database —
/// because the *credentials* may legitimately differ while still
/// naming the same database.
pub(crate) fn refuse_registry_url(tenant_url: &str, registry_url: &str) -> Result<(), String> {
    if endpoint_identity(tenant_url) == endpoint_identity(registry_url) {
        return Err(format!(
            "this is the registry's own database ({}). A tenant needs its own — pointing one \
             here would run the tenant migrations over the registry",
            crate::sql::connect_diagnosis::redact(tenant_url)
        ));
    }
    Ok(())
}

/// Build a tenant URL on the same server as the registry, naming
/// `database`.
///
/// The overwhelmingly common deployment is "one Postgres, one database
/// per tenant". Without this, an operator retypes the host, the port
/// **and the password** into a web form for every tenant — which is
/// both tedious and the single most likely way for a credential to end
/// up somewhere it should not be.
///
/// Derived server-side on purpose: the caller supplies a database
/// *name*, never a URL, so the registry password is never rendered into
/// a page and never travels back in a form post.
///
/// Returns `None` when the registry URL has no database segment to
/// replace — a shape this cannot reason about, where the operator
/// should supply a URL themselves.
#[must_use]
pub fn tenant_url_on_registry_server(registry_url: &str, database: &str) -> Option<String> {
    // sqlite is a file path, not a server: "same server, other
    // database" means a sibling file.
    if let Some(path) = registry_url.strip_prefix("sqlite://") {
        let path = path.split('?').next().unwrap_or(path);
        let (dir, _) = path.rsplit_once('/')?;
        return Some(format!("sqlite://{dir}/{database}.db?mode=rwc"));
    }

    let (scheme, rest) = registry_url.split_once("://")?;
    // Keep userinfo and authority; replace only the path segment.
    let (authority, _old_db) = rest.rsplit_once('/')?;
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}/{database}"))
}

/// Scheme + host + port + database, lowercased, credentials and query
/// dropped. Two URLs naming the same database compare equal even when
/// they authenticate differently.
fn endpoint_identity(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or_else(|| {
        // `sqlite:path` has no authority — the whole tail is the file.
        url.split_once(':').unwrap_or(("", url))
    });
    // Drop userinfo (everything before the last `@` of the authority).
    let rest = rest.rsplit_once('@').map_or(rest, |(_, after)| after);
    // Drop the query string: `?mode=rwc` does not change which database
    // this is.
    let rest = rest.split_once('?').map_or(rest, |(before, _)| before);
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        rest.to_ascii_lowercase()
    )
}

/// The schema a schema-mode tenant lives in: whatever the request
/// named, or the slug. `None` in database-mode, which has no schema.
fn schema_name_for(request: &ProvisionRequest) -> Option<String> {
    match request.mode {
        StorageMode::Schema => Some(
            request
                .schema_name
                .clone()
                .unwrap_or_else(|| request.slug.clone()),
        ),
        StorageMode::Database => None,
    }
}

/// The registry row for a validated request, slug-derived defaults
/// filled in.
///
/// `host_pattern` is the interesting one: an unset pattern becomes
/// `<slug>.<RUSTANGO_APEX_DOMAIN>` when that env var is set, and stays
/// unset when it is not — a tenant with no host pattern simply does not
/// resolve by subdomain.
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
        host_pattern: request.host_pattern.clone().or_else(|| {
            std::env::var("RUSTANGO_APEX_DOMAIN")
                .ok()
                .map(|apex| format!("{}.{apex}", request.slug))
        }),
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

/// `CREATE SCHEMA IF NOT EXISTS` on the registry.
///
/// Schema-mode is Postgres-only **by language**: `CREATE SCHEMA` and
/// `SET search_path` do not exist on `SQLite` or `MySQL`. So there are
/// two distinct refusals, and they say different things:
///
/// * this build has no `postgres` feature at all, or
/// * it does, but these `TenantPools<DB>` are not holding a PG pool —
///   which only a runtime downcast can tell us, since `DB` is generic.
async fn provision_schema<DB: Database>(
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
                    "schema-mode tenants require a Postgres registry — pass --mode database \
                     on sqlite/mysql"
                        .into(),
                )
            })?;
        // The dialect's own quoter, not a local copy: it is the
        // canonical one, and it doubles any embedded `"` so a schema
        // name straight from a request survives quoting intact.
        let sql = format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            crate::sql::Postgres.quote_ident(schema)
        );
        rustango::sql::sqlx::query(&sql)
            .execute(pg_pools.registry())
            .await?;
        Ok(())
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = (pools, schema);
        Err(TenancyError::Validation(
            "schema-mode tenants require the `postgres` feature — pass --mode database \
             on sqlite/mysql builds"
                .into(),
        ))
    }
}

/// Flip the tenant live.
async fn activate(registry: &crate::sql::Pool, org_id: i64) -> Result<(), TenancyError> {
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

/// Migrate the tenant that was just created — and only that one.
///
/// Goes through [`tenant_migrate::migrate_one_tenant`] rather than the
/// batch, for two reasons. The tenant is still inactive at this point,
/// so the batch (which filters `active = true`) would skip the very
/// tenant it was called for. And the batch would migrate every *other*
/// tenant too, so creating tenant B did a pass over tenant A and an
/// unrelated broken chain surfaced mid-provision.
///
/// Never returns `Err`: by this point the `Org` row exists, and an
/// error return would hide that from the caller. A failure comes back
/// as [`MigrationsOutcome::Failed`].
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

    /// The registry's own connection URL.
    ///
    /// Used to derive a tenant URL on the same server — the common
    /// case, and the one that otherwise has an operator retyping a
    /// password into a form field. **Never render this**: it carries
    /// credentials. Derive, then redact for display.
    /// Migrate one tenant (`slug`) or every active one (`None`),
    /// recording into an already-open run.
    ///
    /// Here rather than on a trait of its own because this is the same
    /// type erasure: the console holds `Arc<dyn TenantProvisioner>` and
    /// has no `DB` to name.
    fn migrate_in_run<'a>(&'a self, run_id: i64, slug: Option<&'a str>) -> BoxFuture<'a, ()>;

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

    #[test]
    fn an_over_long_slug_is_refused_at_the_hostname_limit() {
        assert!(validate_slug(&"a".repeat(63)).is_ok());
        let err = validate_slug(&"a".repeat(64)).expect_err("too long for a hostname label");
        assert!(err.contains("63"), "{err}");
    }

    /// The critical QA finding: a tenant pointed at the registry's own
    /// database ran the tenant migration chain over the registry.
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
}
