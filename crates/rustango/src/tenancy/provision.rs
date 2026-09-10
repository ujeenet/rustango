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
//! 2. [`CheckConnection`](ProvisionStep::CheckConnection) — **not
//!    implemented yet** (#1319). It reports
//!    [`Skipped`](StepStatus::Skipped) so the shape is visible in the
//!    stream from the start, rather than appearing later and changing
//!    what a watcher has to handle.
//! 3. [`ProvisionStorage`](ProvisionStep::ProvisionStorage) —
//!    `CREATE SCHEMA` for schema-mode. Deliberately before the row
//!    lands, so a failed `INSERT` leaves no orphan schema.
//! 4. [`RegisterOrg`](ProvisionStep::RegisterOrg) — the `rustango_orgs`
//!    row, after which the tenant resolves.
//! 5. [`Migrate`](ProvisionStep::Migrate) — the tenant's own schema.
//!
//! ## What is deliberately still wrong here
//!
//! Step 5 migrates **every active tenant**, then filters the report down
//! to the one just created. That is what the verb has always done, and
//! changing it is not this refactor's business — but it means creating
//! tenant B does a pass over tenant A, and an unrelated tenant's broken
//! chain shows up in the middle of an unrelated provisioning run. See
//! the parent epic.

use std::path::Path;

use sqlx::Database;

use crate::core::Column as _;
use crate::migrate::file::Migration;
use crate::sql::{Auto, FetcherPool};

use super::error::TenancyError;
use super::migrate as tenant_migrate;
use super::org::{BackendKind, Org, StorageMode};
use super::pools::TenantPools;

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
        }
    }
}

/// One stage of provisioning, reportable on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionStep {
    /// Slug is free; mode, backend and build agree.
    Validate,
    /// Reach the target database before writing anything. Not
    /// implemented yet — see #1319.
    CheckConnection,
    /// `CREATE SCHEMA` for schema-mode. Nothing to do in database-mode.
    ProvisionStorage,
    /// Insert the `rustango_orgs` row.
    RegisterOrg,
    /// Apply the tenant's migrations.
    Migrate,
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

fn emit(observer: Option<&dyn ProvisionObserver>, event: impl FnOnce() -> ProvisionEvent) {
    if let Some(observer) = observer {
        observer.on_event(event());
    }
}

fn step(observer: Option<&dyn ProvisionObserver>, step: ProvisionStep, status: StepStatus) {
    emit(observer, || ProvisionEvent::Step { step, status });
}

/// Report a step's failure to the observer, then return the error.
///
/// Every failure path goes through this rather than a bare `?`, so a
/// watcher never sees a step stuck on `Started` with no explanation —
/// which is the state the whole event stream exists to prevent.
fn fail<T>(
    observer: Option<&dyn ProvisionObserver>,
    at: ProvisionStep,
    e: TenancyError,
) -> Result<T, TenancyError> {
    step(observer, at, StepStatus::Failed(e.to_string()));
    Err(e)
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
    // ---- 1. Validate ----
    step(observer, ProvisionStep::Validate, StepStatus::Started);
    let registry = pools.registry_pool();

    // Reject a duplicate slug up front — it saves a partial-state mess
    // where `CREATE SCHEMA` succeeds and the `INSERT` then fails.
    let existing: Vec<Org> = match Org::objects()
        .where_(Org::slug.eq(request.slug.clone()))
        .fetch(&registry)
        .await
    {
        Ok(rows) => rows,
        Err(e) => return fail(observer, ProvisionStep::Validate, e.into()),
    };
    if !existing.is_empty() {
        return fail(
            observer,
            ProvisionStep::Validate,
            TenancyError::Validation(format!("tenant slug `{}` already exists", request.slug)),
        );
    }

    if request.mode == StorageMode::Database && request.database_url.is_none() {
        return fail(
            observer,
            ProvisionStep::Validate,
            TenancyError::Validation(
                "create-tenant --mode database requires --database-url".into(),
            ),
        );
    }
    step(observer, ProvisionStep::Validate, StepStatus::Ok);

    // ---- 2. Check the connection ----
    //
    // The step exists in the stream from day one even though it does
    // nothing: a watcher written against today's events keeps working
    // when #1319 fills it in, instead of suddenly meeting a stage it
    // has never seen.
    step(
        observer,
        ProvisionStep::CheckConnection,
        StepStatus::Skipped("connection pre-flight is not implemented yet (#1319)".into()),
    );

    let schema_name = schema_name_for(request);

    // ---- 3. Provision storage ----
    //
    // Before the row, not after: a failed `INSERT` must not leave an
    // orphan schema behind. Idempotent via `IF NOT EXISTS`.
    match request.mode {
        StorageMode::Schema => {
            step(
                observer,
                ProvisionStep::ProvisionStorage,
                StepStatus::Started,
            );
            let schema = schema_name.as_deref().unwrap_or(&request.slug);
            if let Err(e) = provision_schema(pools, schema).await {
                return fail(observer, ProvisionStep::ProvisionStorage, e);
            }
            step(observer, ProvisionStep::ProvisionStorage, StepStatus::Ok);
        }
        StorageMode::Database => step(
            observer,
            ProvisionStep::ProvisionStorage,
            StepStatus::Skipped("database-mode tenants bring their own database".into()),
        ),
    }

    // ---- 4. Register the org ----
    step(observer, ProvisionStep::RegisterOrg, StepStatus::Started);
    let mut org = new_org_row(request, schema_name);
    if let Err(e) = org.insert_pool(&registry).await {
        return fail(observer, ProvisionStep::RegisterOrg, e.into());
    }
    // This pod sees the new tenant immediately; others converge on the
    // registry fingerprint (see `resolver::sync_org_generation`).
    super::invalidate_org_cache();
    let org_id = org.id.get().copied().unwrap_or_default();
    step(observer, ProvisionStep::RegisterOrg, StepStatus::Ok);
    emit(observer, || ProvisionEvent::Registered { org_id });

    // ---- 5. Migrate ----
    let migrations = if request.run_migrations {
        step(observer, ProvisionStep::Migrate, StepStatus::Started);
        let outcome = migrate_new_tenant(pools, registry_url, dir, &request.slug, observer).await?;
        match &outcome {
            MigrationsOutcome::Failed(e) => {
                step(
                    observer,
                    ProvisionStep::Migrate,
                    StepStatus::Failed(e.clone()),
                );
            }
            _ => step(observer, ProvisionStep::Migrate, StepStatus::Ok),
        }
        outcome
    } else {
        step(
            observer,
            ProvisionStep::Migrate,
            StepStatus::Skipped("caller asked for no migrations".into()),
        );
        MigrationsOutcome::Skipped
    };

    Ok(ProvisionOutcome {
        org_id,
        slug: request.slug.clone(),
        mode: request.mode,
        migrations,
    })
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
        active: true,
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

/// Migrate the tenant that was just created.
///
/// Runs the whole active-tenant batch and then picks this slug out of
/// the report — see the module docs for why that is more work than it
/// should be.
async fn migrate_new_tenant<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    slug: &str,
    observer: Option<&dyn ProvisionObserver>,
) -> Result<MigrationsOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // Forward every migration event straight through, so a caller
    // watching a provisioning run sees the same per-migration detail
    // `manage migrate` prints (#1320).
    let forward = observer.map(|observer| {
        move |event: tenant_migrate::TenantMigrationEvent| {
            observer.on_event(ProvisionEvent::Migration(event));
        }
    });
    let forward = forward
        .as_ref()
        .map(|f| f as &dyn tenant_migrate::TenantMigrationObserver);

    // v0.38 — on PG go through `migrate_tenants` (schema-mode +
    // database-mode); on sqlite/mysql use `migrate_tenants_db`
    // (database-mode only).
    #[cfg(feature = "postgres")]
    let report = {
        if let Some(pg_pools) =
            (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
        {
            match forward {
                Some(o) => {
                    tenant_migrate::migrate_tenants_with_progress(pg_pools, dir, registry_url, o)
                        .await?
                }
                None => tenant_migrate::migrate_tenants(pg_pools, dir, registry_url).await?,
            }
        } else {
            match forward {
                Some(o) => {
                    tenant_migrate::migrate_tenants_db_with_progress(pools, dir, registry_url, o)
                        .await?
                }
                None => tenant_migrate::migrate_tenants_db(pools, dir, registry_url).await?,
            }
        }
    };
    #[cfg(not(feature = "postgres"))]
    let report = match forward {
        Some(o) => {
            tenant_migrate::migrate_tenants_db_with_progress(pools, dir, registry_url, o).await?
        }
        None => tenant_migrate::migrate_tenants_db(pools, dir, registry_url).await?,
    };

    Ok(match report.tenants.into_iter().find(|t| t.slug == slug) {
        Some(o) => match o.error {
            Some(e) => MigrationsOutcome::Failed(e.to_string()),
            None => MigrationsOutcome::Applied(o.applied),
        },
        None => MigrationsOutcome::NotMatched,
    })
}
