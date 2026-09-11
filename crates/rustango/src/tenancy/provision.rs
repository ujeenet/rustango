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
    use super::provision_store::{self as store, RunState};

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

    let refreshed = store::run_by_id(&registry, run_id).await?.unwrap_or(run);
    result.map(|outcome| (refreshed, outcome))
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

    if request.mode == StorageMode::Database && request.database_url.is_none() {
        return rep
            .fail(
                ProvisionStep::Validate,
                TenancyError::Validation(
                    "create-tenant --mode database requires --database-url".into(),
                ),
            )
            .await;
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
