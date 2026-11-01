//! Scoped migration runners — registry vs tenant.
//!
//! Migration files carry a `scope: registry | tenant` field
//! ([`rustango::migrate::MigrationScope`]); slice 1 added the field,
//! slice 3 (this module) wires it into routing.
//!
//! ## Registry migrations
//!
//! [`migrate_registry`] applies migrations marked
//! `scope = "registry"` to the registry DB. Tenant-scoped migrations
//! are skipped. Only the registry pool is touched. Idempotent +
//! ledger-tracked through the standard `__rustango_migrations__`
//! table in the registry's `public` schema.
//!
//! ## Tenant migrations
//!
//! [`migrate_tenants`] walks every active org from the registry,
//! resolves its pool via [`TenantPools`], and applies migrations
//! marked `scope = "tenant"` to that tenant's storage:
//!
//! * **Schema mode** — the runner spins up an *ephemeral* PgPool
//!   bound to the registry URL with an `after_connect` hook that
//!   issues `SET search_path TO <schema>, public` on every fresh
//!   connection. The migration runner from `rustango-migrate` runs
//!   unchanged against this pool; its ledger queries
//!   (`__rustango_migrations__`) resolve to `<schema>.__rustango_migrations__`
//!   thanks to search_path. Each tenant gets its own ledger row set.
//!   The ephemeral pool is dropped after the tenant finishes; we
//!   do not reuse it as the runtime tenant pool because runtime
//!   uses the shared registry pool with per-checkout `SET`
//!   (whereas migration wants connection-level `SET` for transaction
//!   safety).
//!
//! * **Database mode** — the runner uses the tenant's dedicated
//!   pool (built lazily through [`TenantPools::pool_for_org`]). The
//!   ledger lives at `public.__rustango_migrations__` in the tenant's
//!   own database — single-schema, no `search_path` dance.
//!
//! ## Failure isolation
//!
//! [`migrate_tenants`] does **not** abort the whole batch when one
//! tenant fails. Each tenant's outcome (applied migrations, secrets
//! errors, SQL errors) lands in [`TenantMigrationReport`]. The caller
//! decides what to surface — an operator dashboard / log digest /
//! blocking error in CI. The registry connection URL is currently
//! reconstructed from the registry pool; a future hook could let
//! callers supply it explicitly when their connection-string layout
//! demands.

use crate::migrate::{MigrateError, Migration, MigrationScope};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::core::Column as _;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;
use sqlx::Database;

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::pools::TenantPools;

/// Outcome of [`migrate_tenants`].
#[derive(Debug, Default)]
pub struct TenantMigrationReport {
    /// One entry per tenant, in the order they were processed.
    pub tenants: Vec<TenantMigrationOutcome>,
}

impl TenantMigrationReport {
    /// `true` when every tenant migrated cleanly.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.tenants.iter().all(|t| t.error.is_none())
    }

    /// Number of tenants that errored.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.tenants.iter().filter(|t| t.error.is_some()).count()
    }
}

/// Which of a tenant's two migration chains an event came from.
///
/// A tenant migrates twice: the framework's own `system/migrations/`
/// chain (its tables — users, roles, permissions, api_keys, audit_log,
/// content_types) into the `__rustango_system_migrations__` ledger, then
/// the project's own chain into `__rustango_migrations__`. They are
/// numbered independently, so **`0001_initial` legitimately appears
/// twice in one tenant's run**. Without this a watcher sees the same
/// name land twice and concludes something re-ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    /// The framework's own tables, applied first — the project's
    /// migrations may FK into them (#1171).
    System,
    /// The project's own migrations.
    Project,
}

/// Something that happened while migrating a set of tenants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantMigrationEvent {
    /// The active-tenant set has been read. Emitted once, up front.
    Planned {
        /// How many tenants will be migrated.
        tenants: usize,
    },
    /// Starting this tenant.
    TenantStarted {
        slug: String,
        /// 1-based.
        index: usize,
        total: usize,
    },
    /// A migration-level event from inside one tenant's run.
    Migration {
        slug: String,
        chain: Chain,
        event: crate::migrate::MigrationEvent,
    },
    /// This tenant is done. Unlike the migration runner, a failure here
    /// is **not** terminal: `migrate_tenants_db` deliberately continues
    /// with the remaining tenants, so more events follow.
    TenantFinished {
        slug: String,
        index: usize,
        total: usize,
        /// Migrations applied across both chains.
        applied: usize,
        /// `Some` if this tenant failed; the batch continues regardless.
        error: Option<String>,
    },
}

/// Receives [`TenantMigrationEvent`]s as a tenant batch progresses.
///
/// Implemented for any `Fn(TenantMigrationEvent)`, so a closure works
/// directly. **Must not block** — the inner migration events are emitted
/// with that tenant's migrate lock held; see
/// [`crate::migrate::progress`].
pub trait TenantMigrationObserver: Send + Sync {
    /// Handle one event. Must return promptly and must not panic.
    fn on_event(&self, event: TenantMigrationEvent);
}

impl<F> TenantMigrationObserver for F
where
    F: Fn(TenantMigrationEvent) + Send + Sync,
{
    fn on_event(&self, event: TenantMigrationEvent) {
        self(event);
    }
}

/// Adapt a tenant observer into a migration observer for one tenant's
/// one chain, tagging every event with the slug and chain it came from.
fn chain_observer<'a>(
    observer: Option<&'a dyn TenantMigrationObserver>,
    slug: &'a str,
    chain: Chain,
) -> Option<impl crate::migrate::MigrationObserver + 'a> {
    observer.map(move |observer| {
        move |event: crate::migrate::MigrationEvent| {
            observer.on_event(TenantMigrationEvent::Migration {
                slug: slug.to_owned(),
                chain,
                event,
            });
        }
    })
}

/// Per-tenant migration outcome.
#[derive(Debug)]
pub struct TenantMigrationOutcome {
    pub slug: String,
    /// Migrations newly applied to this tenant.
    pub applied: Vec<Migration>,
    /// `Some(_)` if the tenant errored; the rest of the batch
    /// continues regardless. The operator dashboard / CI step
    /// should surface this.
    pub error: Option<TenancyError>,
}

/// Ledger table tracking the framework's own ("system app") migrations,
/// kept separate from the project's `__rustango_migrations__` so the two
/// chains never collide.
pub(crate) const SYSTEM_LEDGER: &str = "__rustango_system_migrations__";

/// Apply `chain`'s `scope` steps and the project chain via `project` to
/// `pool`, in [`crate::migrate::chains::migrate_chains`]'s order.
///
/// The caller prepares `chain` once per run from the project root, never
/// from a scoped temp copy or an app folder, so every tenant sees the
/// committed `system/migrations/` and the same origin (#1988).
async fn migrate_with_system<F, Fut>(
    pool: &crate::sql::Pool,
    chain: &crate::migrate::make::SystemChain,
    scope: MigrationScope,
    dirs: &[PathBuf],
    observer: Option<&dyn crate::migrate::MigrationObserver>,
    signals: crate::migrate::Signals<'_>,
    project: F,
) -> Result<Vec<Migration>, TenancyError>
where
    F: FnOnce(crate::migrate::LockHeld) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Migration>, crate::migrate::MigrateError>>,
{
    let applied = crate::migrate::chains::migrate_chains(
        pool, chain, scope, dirs, observer, signals, project,
    )
    .await?;
    let mut all = applied.system;
    all.extend(applied.project);
    Ok(all)
}

/// Each folder's project chain, in order, through `run`. A failure names
/// its folder and what the folders before it applied.
async fn each_dir<'d, G, Fut>(
    project: &'d ScopedProject,
    mut run: G,
) -> Result<Vec<Migration>, MigrateError>
where
    G: FnMut(&'d Path) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Migration>, MigrateError>>,
{
    let mut all: Vec<Migration> = Vec::new();
    for (source, dir) in project.sources.iter().zip(&project.dirs) {
        match run(dir).await {
            Ok(applied) => all.extend(applied),
            Err(e) if project.dirs.len() == 1 => return Err(e),
            Err(e) => {
                let before = if all.is_empty() {
                    String::new()
                } else {
                    let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
                    format!("; applied before it: {}", names.join(", "))
                };
                return Err(MigrateError::Validation(format!(
                    "{}: {e}{before}",
                    source.display()
                )));
            }
        }
    }
    Ok(all)
}

/// A project's migration dirs cut down to one scope, in apply order, and
/// the root its one system chain lives under.
pub(crate) struct ScopedProject {
    root: PathBuf,
    /// The folders as on disk, for messages.
    sources: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    // Keeps the temp copies `dirs` points into alive.
    _copies: Vec<ScopedDir>,
}

impl ScopedProject {
    /// `dir`'s project: its `migrations/` and `<app>/migrations/` folders,
    /// each cut down to `scope`.
    pub(crate) async fn of(dir: &Path, scope: MigrationScope) -> Result<Self, TenancyError> {
        let mut dirs = Vec::new();
        let mut copies = Vec::new();
        let sources = crate::migrate::file::project_migration_dirs(dir);
        for source in &sources {
            let copy = scoped_subset(source, scope).await?;
            dirs.push(copy.path(source).to_path_buf());
            copies.push(copy);
        }
        Ok(Self {
            root: crate::migrate::file::project_root_of(dir).to_path_buf(),
            sources,
            dirs,
            _copies: copies,
        })
    }

    pub(crate) fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The project's system chain for `scope`, from its root: an app
    /// folder never gets a `system/` of its own.
    fn system_chain(
        &self,
        scope: crate::core::ModelScope,
    ) -> Result<crate::migrate::make::SystemChain, MigrateError> {
        crate::migrate::make::SystemChain::prepare(&self.root, &[scope])
    }
}

/// Apply registry-scoped pending migrations to the registry DB.
///
/// Only migrations whose `scope == Registry` run. Tenant-scoped
/// migrations are silently skipped here — they're for
/// [`migrate_tenants`].
///
/// # Errors
/// As [`crate::migrate::migrate`].
/// v0.38 — generic over the registry backend. Routes through the
/// tri-dialect [`migrate_registry_pool`].
pub async fn migrate_registry<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    migrate_registry_pool(&pools.registry_pool(), dir).await
}

/// Backend-agnostic registry migration runner — counterpart of
/// [`migrate_registry`] that takes a [`crate::sql::Pool`] enum
/// directly instead of going through [`TenantPools`]. Runs the
/// migrations, the `managed = false` table bootstrap and the
/// contenttype seed on any backend.
///
/// The PG-only password-changed-at ALTER stays gated to Postgres —
/// it only matters for registries upgraded from pre-v0.28.4, and
/// fresh non-PG registries are never in that state.
///
/// # Errors
/// As [`crate::migrate::migrate_pool`].
pub async fn migrate_registry_pool(
    registry: &crate::sql::Pool,
    dir: &Path,
) -> Result<Vec<Migration>, TenancyError> {
    info!(target: "rustango::tenancy", "applying registry-scoped migrations");
    let project = ScopedProject::of(dir, MigrationScope::Registry).await?;
    // Model-derived registry tables come from system-app migrations; the
    // `managed = false` ones from `ensure_bootstrap_tables` below.
    let chain = project.system_chain(crate::core::ModelScope::Registry)?;
    let applied = migrate_with_system(
        registry,
        &chain,
        MigrationScope::Registry,
        project.dirs(),
        None,
        crate::migrate::Signals::Skip,
        |held| {
            each_dir(&project, move |d| {
                crate::migrate::migrate_pool_locked(held, registry, d, None)
            })
        },
    )
    .await?;
    registry_after_migrate(registry).await?;
    info!(
        target: "rustango::tenancy",
        applied = applied.len(),
        "registry migrations done"
    );
    Ok(applied)
}

/// What the registry gets after its chains run, whichever way they ran.
async fn registry_after_migrate(registry: &crate::sql::Pool) -> Result<(), TenancyError> {
    // Tenancy's `migrate` never reaches the single-database path (#2360).
    crate::migrate::manage::ensure_bootstrap_tables(registry).await?;
    // (#89) Auto-seed the `rustango_content_types` registry-side
    // catalog — the operator console's audit log + permissions UI
    // consult it to resolve `entity_table` strings back to a stable
    // per-model identifier. Bi-dialect via
    // `contenttypes::ensure_seeded` (v0.34 slice 1).
    if let Err(e) = crate::contenttypes::ensure_seeded(registry).await {
        tracing::warn!(
            target: "rustango::tenancy",
            error = %e,
            "contenttypes::ensure_seeded failed for registry pool",
        );
    }
    // #1464 — the single-database `migrate` runs this at the end of its
    // own path, and tenancy never reaches that: `tenancy::manage`
    // intercepts the `migrate` verb before the fall-through, precisely
    // so migrations stay scope-aware. That left the registry and every
    // tenant database un-normalised while this release switched the
    // write side to RFC3339 — the new assumption with none of the
    // repair, reported as success.
    normalise_datetimes_quietly(registry, "registry").await;
    Ok(())
}

/// What a forward `migrate <target>` did on the registry.
pub(crate) struct MovedTo {
    pub(crate) system: Vec<Migration>,
    pub(crate) applied: Vec<Migration>,
    pub(crate) unapplied: Vec<Migration>,
}

/// `migrate <target>` forward on the registry: the project's registry-scope
/// system chain, from its root, then the folder holding `target` moved to
/// it. `None` when no registry-scoped file is named `target`, or the ledger
/// already has it.
///
/// # Errors
/// Either chain's error, or the bootstrap after them.
pub(crate) async fn migrate_registry_to(
    registry: &crate::sql::Pool,
    dir: &Path,
    target: &str,
) -> Result<Option<MovedTo>, TenancyError> {
    let project = ScopedProject::of(dir, MigrationScope::Registry).await?;
    let holds = |d: &PathBuf| {
        rustango::migrate::file::list_dir(d).is_ok_and(|all| all.iter().any(|m| m.name == target))
    };
    let Some(target_dir) = project.dirs().iter().find(|d| holds(d)) else {
        return Ok(None);
    };
    let chain = project.system_chain(crate::core::ModelScope::Registry)?;
    let Some(ran) = crate::migrate::chains::migrate_chains_to(
        registry,
        &chain,
        MigrationScope::Registry,
        project.dirs(),
        crate::migrate::Signals::Skip,
        target_dir,
        target,
    )
    .await?
    else {
        return Ok(None);
    };
    registry_after_migrate(registry).await?;
    Ok(Some(MovedTo {
        system: ran.system,
        applied: ran.project,
        unapplied: ran.unapplied,
    }))
}

/// Apply tenant-scoped pending migrations to every active org.
///
/// Walks `Org::objects().where_(active = true)`, resolves each
/// tenant's storage, and applies every `scope == Tenant` migration
/// from `dir` to that tenant's ledger. Per-tenant atomicity (each
/// migration in its own tx by default); per-tenant failure isolation
/// (one tenant's bad migration doesn't block the rest).
///
/// `registry_url` is the connection string used to spin up the
/// short-lived per-tenant pools for schema-mode tenants. Database-mode
/// tenants use the cached pool from [`TenantPools`] directly; the URL
/// is only needed for schema mode.
///
/// # Errors
/// Walking the Org table or building the scoped subset can short-
/// circuit; returns `Err` in those cases. Per-tenant errors are
/// captured in the [`TenantMigrationReport`] without aborting.
#[cfg(feature = "postgres")]
pub async fn migrate_tenants(
    pools: &TenantPools,
    dir: &Path,
    registry_url: &str,
) -> Result<TenantMigrationReport, TenancyError> {
    migrate_tenants_opts(pools, dir, registry_url, None).await
}

/// [`migrate_tenants`], reporting progress to `observer` as it goes.
///
/// The Postgres sibling of [`migrate_tenants_db_with_progress`], and the
/// one the CLI actually reaches on a PG registry — it handles
/// schema-mode tenants as well as database-mode.
///
/// **The observer must not block**; see [`crate::migrate::progress`].
///
/// # Errors
/// As [`migrate_tenants`].
#[cfg(feature = "postgres")]
pub async fn migrate_tenants_with_progress(
    pools: &TenantPools,
    dir: &Path,
    registry_url: &str,
    observer: &dyn TenantMigrationObserver,
) -> Result<TenantMigrationReport, TenancyError> {
    migrate_tenants_opts(pools, dir, registry_url, Some(observer)).await
}

#[cfg(feature = "postgres")]
async fn migrate_tenants_opts(
    pools: &TenantPools,
    dir: &Path,
    registry_url: &str,
    observer: Option<&dyn TenantMigrationObserver>,
) -> Result<TenantMigrationReport, TenancyError> {
    let project = ScopedProject::of(dir, MigrationScope::Tenant).await?;
    let chain = project.system_chain(crate::core::ModelScope::Tenant)?;
    let migrations_in_scope = rustango::migrate::file::list_each(project.dirs())
        .map(|m| m.len())
        .unwrap_or(0);

    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch_on(pools.registry())
        .await?;

    info!(
        target: "rustango::tenancy",
        tenants = orgs.len(),
        migrations = migrations_in_scope,
        dir = %dir.display(),
        "applying tenant-scoped migrations"
    );
    if migrations_in_scope == 0 && !orgs.is_empty() {
        // Surface the most likely cause for an `applied=0` report: caller
        // passed a path with no tenant-scoped migrations. Common footgun
        // is passing the project root when a flat `migrations/` subdir
        // was meant. Tenant provisioning auto-detects via
        // `provision::migration_dirs`, but raw callers can still hit this.
        warn!(
            target: "rustango::tenancy",
            dir = %dir.display(),
            "no tenant-scoped migrations found in dir; tenants will record applied=0 — \
             pass the flat migrations directory or a project root containing one"
        );
    }

    let total = orgs.len();
    if let Some(observer) = observer {
        observer.on_event(TenantMigrationEvent::Planned { tenants: total });
    }

    let mut report = TenantMigrationReport::default();
    for (i, org) in orgs.iter().enumerate() {
        let index = i + 1;
        if let Some(observer) = observer {
            observer.on_event(TenantMigrationEvent::TenantStarted {
                slug: org.slug.clone(),
                index,
                total,
            });
        }
        let outcome =
            run_for_one_tenant(pools, org, &project, &chain, registry_url, observer).await;
        if let Some(observer) = observer {
            observer.on_event(TenantMigrationEvent::TenantFinished {
                slug: org.slug.clone(),
                index,
                total,
                applied: outcome.as_ref().map_or(0, Vec::len),
                error: outcome.as_ref().err().map(ToString::to_string),
            });
        }
        match &outcome {
            Ok(applied) => info!(
                target: "rustango::tenancy",
                slug = %org.slug,
                applied = applied.len(),
                "tenant migrations done"
            ),
            Err(e) => warn!(
                target: "rustango::tenancy",
                slug = %org.slug,
                error = %e,
                "tenant migration failed; continuing with remaining tenants"
            ),
        }
        report.tenants.push(match outcome {
            Ok(applied) => TenantMigrationOutcome {
                slug: org.slug.clone(),
                applied,
                error: None,
            },
            Err(error) => TenantMigrationOutcome {
                slug: org.slug.clone(),
                applied: Vec::new(),
                error: Some(error),
            },
        });
    }
    Ok(report)
}

/// v0.38 — tri-dialect counterpart of [`migrate_tenants`]. Walks
/// active orgs from the registry (any backend) and applies
/// tenant-scoped migrations. Schema-mode tenants are rejected
/// (schema-mode is PG-only by language); database-mode tenants
/// migrate against their per-tenant pool via
/// [`TenantPools::database_acquire`].
///
/// `_registry_url` is accepted for API symmetry with
/// [`migrate_tenants`] but unused on this path — database-mode
/// migrations don't need to spin up a schema-scoped pool.
///
/// # Errors
/// Walking the Org table can short-circuit; per-tenant errors are
/// captured in the [`TenantMigrationReport`] without aborting.
pub async fn migrate_tenants_db<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    migrate_tenants_db_opts(pools, dir, registry_url, None).await
}

/// [`migrate_tenants_db`], reporting progress to `observer` as it goes.
///
/// The observer sees the tenant count up front, then per tenant a
/// start, every migration event from both of that tenant's chains (see
/// [`Chain`]), and a finish carrying that tenant's error if it had one.
/// A tenant failure does not end the stream — the batch continues, and
/// so do the events.
///
/// **The observer must not block**: migration events are emitted with
/// the tenant's migrate lock held. See [`crate::migrate::progress`].
///
/// # Errors
/// As [`migrate_tenants_db`]. Per-tenant failures are reported in the
/// returned [`TenantMigrationReport`], not as an error.
pub async fn migrate_tenants_db_with_progress<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
    observer: &dyn TenantMigrationObserver,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    migrate_tenants_db_opts(pools, dir, registry_url, Some(observer)).await
}

/// Migrate exactly one tenant, named by its `Org` row.
///
/// The batch entry points walk `Org::objects().where_(active = true)`,
/// which makes them the wrong tool twice over for a tenant that is
/// being stood up:
///
/// * a tenant mid-provision is deliberately **inactive** until its
///   schema is in place, so the batch would skip the very tenant it was
///   called for; and
/// * running the whole batch means creating tenant B does a pass over
///   tenant A, and an unrelated tenant's broken chain surfaces in the
///   middle of an unrelated provisioning run.
///
/// Takes the `Org` directly and ignores `active` — the caller has
/// already decided this is the tenant it means.
///
/// # Errors
/// Anything the tenant's own migration run can raise: an unreachable
/// tenant database, a failing migration, or a storage mode this build
/// cannot serve.
pub async fn migrate_one_tenant<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    dir: &Path,
    registry_url: &str,
    observer: Option<&dyn TenantMigrationObserver>,
) -> Result<Vec<Migration>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let project = ScopedProject::of(dir, MigrationScope::Tenant).await?;
    let chain = project.system_chain(crate::core::ModelScope::Tenant)?;

    // Schema-mode is PG-only by language, and only the PG runner knows
    // how to build a `search_path`-scoped pool for it. Route the same
    // way the batch does.
    #[cfg(feature = "postgres")]
    if matches!(
        StorageMode::parse(&org.storage_mode),
        Ok(StorageMode::Schema)
    ) {
        let pg_pools = (pools as &dyn std::any::Any)
            .downcast_ref::<TenantPools<sqlx::Postgres>>()
            .ok_or_else(|| {
                TenancyError::Validation(format!(
                    "org `{}` is schema-mode but the registry is not Postgres",
                    org.slug
                ))
            })?;
        return run_for_one_tenant(pg_pools, org, &project, &chain, registry_url, observer).await;
    }
    let _ = registry_url;

    run_for_one_tenant_db(pools, org, &project, &chain, observer).await
}

async fn migrate_tenants_db_opts<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    _registry_url: &str,
    observer: Option<&dyn TenantMigrationObserver>,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::sql::FetcherPool as _;
    let project = ScopedProject::of(dir, MigrationScope::Tenant).await?;
    let chain = project.system_chain(crate::core::ModelScope::Tenant)?;

    let registry_pool = pools.registry_pool();
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&registry_pool)
        .await?;

    info!(
        target: "rustango::tenancy",
        tenants = orgs.len(),
        dir = %dir.display(),
        "applying tenant-scoped migrations (db-mode only)"
    );

    let total = orgs.len();
    if let Some(observer) = observer {
        observer.on_event(TenantMigrationEvent::Planned { tenants: total });
    }

    let mut report = TenantMigrationReport::default();
    for (i, org) in orgs.iter().enumerate() {
        let index = i + 1;
        if let Some(observer) = observer {
            observer.on_event(TenantMigrationEvent::TenantStarted {
                slug: org.slug.clone(),
                index,
                total,
            });
        }
        let outcome = run_for_one_tenant_db(pools, org, &project, &chain, observer).await;
        if let Some(observer) = observer {
            observer.on_event(TenantMigrationEvent::TenantFinished {
                slug: org.slug.clone(),
                index,
                total,
                applied: outcome.as_ref().map_or(0, Vec::len),
                error: outcome.as_ref().err().map(ToString::to_string),
            });
        }
        match &outcome {
            Ok(applied) => info!(
                target: "rustango::tenancy",
                slug = %org.slug,
                applied = applied.len(),
                "tenant migrations done"
            ),
            Err(e) => warn!(
                target: "rustango::tenancy",
                slug = %org.slug,
                error = %e,
                "tenant migration failed; continuing with remaining tenants"
            ),
        }
        report.tenants.push(match outcome {
            Ok(applied) => TenantMigrationOutcome {
                slug: org.slug.clone(),
                applied,
                error: None,
            },
            Err(error) => TenantMigrationOutcome {
                slug: org.slug.clone(),
                applied: Vec::new(),
                error: Some(error),
            },
        });
    }
    Ok(report)
}

async fn run_for_one_tenant_db<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    project: &ScopedProject,
    chain: &crate::migrate::make::SystemChain,
    observer: Option<&dyn TenantMigrationObserver>,
) -> Result<Vec<Migration>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            org.slug
        ))
    })?;
    if !matches!(mode, StorageMode::Database) {
        return Err(TenancyError::Validation(format!(
            "org `{}` is schema-mode but migrate_tenants_db only handles \
             database-mode tenants (schema-mode is PG-only by language)",
            org.slug,
        )));
    }
    // Get a tenant-scoped Pool enum (no SET search_path needed for
    // database-mode), apply migrations through `migrate_pool`, then
    // run the audit/permission/contenttype/api-key DDL backfills
    // through the same backend-agnostic helpers the registry-side
    // bootstrap uses.
    let tenant_pool = pools.database_pool_for_org(org).await?;
    let inner_pool = match &tenant_pool {
        super::pools::TenantPool::Database { pool } => crate::sql::Pool::from((**pool).clone()),
        #[cfg(feature = "postgres")]
        super::pools::TenantPool::Schema { .. } => {
            unreachable!("database_pool_for_org rejects schema-mode")
        }
    };
    // Framework tenant tables (users, roles, permissions, api_keys,
    // audit_log, content_types) come from the makemigrations-generated
    // system-app migrations and MUST be applied BEFORE the tenant's user
    // migrations, which may FK into them (issue #1171).
    let system_progress = chain_observer(observer, &org.slug, Chain::System);
    let project_progress = chain_observer(observer, &org.slug, Chain::Project);
    let applied = migrate_with_system(
        &inner_pool,
        chain,
        MigrationScope::Tenant,
        project.dirs(),
        system_progress
            .as_ref()
            .map(|o| o as &dyn crate::migrate::MigrationObserver),
        crate::migrate::Signals::Fire {
            tenant: Some(&org.slug),
        },
        |held| {
            let observer = project_progress
                .as_ref()
                .map(|o| o as &dyn crate::migrate::MigrationObserver);
            let pool = &inner_pool;
            each_dir(project, move |d| {
                crate::migrate::migrate_pool_locked(held, pool, d, observer)
            })
        },
    )
    .await?;
    crate::migrate::manage::ensure_tenant_bootstrap_tables(&inner_pool).await?;
    // Data seeders (rows, not DDL — kept): the CRUD permission codenames
    // for every registered model (#61) + the content-type catalog (#89).
    if let Err(e) = super::permissions::auto_create_permissions_pool(&inner_pool).await {
        tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "auto_create_permissions_pool failed for database-mode tenant");
    }
    if let Err(e) = crate::contenttypes::ensure_seeded(&inner_pool).await {
        tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for database-mode tenant");
    }
    // #1464 — see the registry seam. SQLite tenancy is
    // database-per-tenant, so a missed tenant is a whole database left
    // holding a shape the ORM no longer compares against.
    normalise_datetimes_quietly(&inner_pool, &org.slug).await;
    Ok(applied)
}

/// Run the #1464 datetime sweep on one pool, warning rather than
/// failing.
///
/// Warn-not-fail matches the seeders either side of it: a migration
/// run that applied every migration has succeeded, and a repair that
/// could not complete should not retract that. The count is logged so
/// it is visible rather than silent — the sweep rewrites stored data,
/// which is not something to do without saying so.
///
/// A no-op on PostgreSQL and MySQL, which have real datetime types.
#[allow(unused_variables)]
async fn normalise_datetimes_quietly(pool: &crate::sql::Pool, scope: &str) {
    #[cfg(feature = "sqlite")]
    match crate::migrate::sqlite_datetime::normalise_sqlite_datetimes(pool).await {
        Ok(fixed) if !fixed.is_clean() => tracing::info!(
            target: "rustango::tenancy",
            scope = %scope,
            rows = fixed.rows,
            columns = ?fixed.columns,
            "normalised SQLite datetime values written before 0.57.11",
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            target: "rustango::tenancy",
            scope = %scope,
            error = %e,
            "SQLite datetime normalisation failed; timestamps written before \
             0.57.11 may not compare correctly until it succeeds",
        ),
    }
}

/// Tri-dialect tenant migration dispatch (v0.38). On PG, downcasts
/// to `TenantPools<sqlx::Postgres>` and calls [`migrate_tenants`]
/// (which handles both schema-mode + database-mode tenants); on any
/// other backend, calls [`migrate_tenants_db`] (database-mode only —
/// schema-mode is PG-only by language).
///
/// Used by [`crate::server::Builder::migrate`] so generic backends
/// share the same Builder entry-point.
///
/// # Errors
/// As [`migrate_tenants`] / [`migrate_tenants_db`].
pub async fn migrate_tenants_dyn<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    migrate_tenants_dyn_with_progress(pools, dir, registry_url, None).await
}

/// [`migrate_tenants_dyn`], reporting progress to `observer`.
///
/// **This is the seam every caller should use.** The
/// "downcast to `TenantPools<Postgres>`, else fall back to the generic
/// runner" dance is easy to write from memory and was written from
/// memory in four places; each copy is a chance to get the schema-mode
/// branch subtly wrong on a non-PG build. One dispatch, here.
///
/// # Errors
/// As [`migrate_tenants`] / [`migrate_tenants_db`].
pub async fn migrate_tenants_dyn_with_progress<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
    observer: Option<&dyn TenantMigrationObserver>,
) -> Result<TenantMigrationReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // On PG the legacy `migrate_tenants` handles schema-mode as well as
    // database-mode; everywhere else `migrate_tenants_db` is the only
    // one that applies, because schema-mode is PG-only by language.
    #[cfg(feature = "postgres")]
    if let Some(pg) = (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>() {
        return migrate_tenants_opts(pg, dir, registry_url, observer).await;
    }
    migrate_tenants_db_opts(pools, dir, registry_url, observer).await
}

#[cfg(all(feature = "postgres", feature = "passkey"))]
const PASSKEY_TABLE: &str = "rustango_webauthn_credentials";

/// `table` in this pool's own schema; a failed probe counts as missing.
#[cfg(all(feature = "postgres", feature = "passkey"))]
async fn existed_here(pool: &crate::sql::Pool, table: &str) -> bool {
    crate::migrate::try_table_exists_here(pool, table)
        .await
        .unwrap_or(false)
}

/// A pre-existing tenant's new passkey table shadows `public`'s on the
/// search path, so passkeys kept there stop working (#2518). Rows are not copied: user
/// ids overlap across tenants, so only the operator knows whose they are.
#[cfg(all(feature = "postgres", feature = "passkey"))]
async fn warn_hidden_public_passkeys(registry: &crate::sql::Pool, slug: &str, schema: &str) {
    use crate::sql::CounterPool as _;
    if !existed_here(registry, PASSKEY_TABLE).await {
        return;
    }
    let rows = crate::passkey::WebauthnCredential::objects()
        .count(registry)
        .await
        .unwrap_or(0);
    if rows > 0 {
        tracing::warn!(
            target: "rustango::tenancy",
            slug,
            rows,
            "tenant `{slug}` now has its own {PASSKEY_TABLE}, which hides the {rows} row(s) in \
             public.{PASSKEY_TABLE}: move this tenant's rows into \"{schema}\".{PASSKEY_TABLE} \
             by hand (see UPGRADING, 0.60.5)"
        );
    }
}

#[cfg(feature = "postgres")]
async fn run_for_one_tenant(
    pools: &TenantPools,
    org: &Org,
    project: &ScopedProject,
    chain: &crate::migrate::make::SystemChain,
    registry_url: &str,
    observer: Option<&dyn TenantMigrationObserver>,
) -> Result<Vec<Migration>, TenancyError> {
    let system_progress = chain_observer(observer, &org.slug, Chain::System);
    let system_progress = system_progress
        .as_ref()
        .map(|o| o as &dyn crate::migrate::MigrationObserver);
    let project_progress = chain_observer(observer, &org.slug, Chain::Project);
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            org.slug
        ))
    })?;
    match mode {
        StorageMode::Schema => {
            let schema = org.effective_schema().to_owned();
            let pool = build_schema_scoped_pool(registry_url, &schema).await?;
            // Framework tenant tables (rustango_users/roles/permissions/…)
            // come from the system-app migrations and MUST be applied
            // BEFORE the tenant's user migrations, which may FK into them
            // (issue #1171). Applying user migrations first breaks a fresh
            // tenant whose model references e.g. rustango_users.
            let dbpool: crate::sql::Pool = pool.clone().into();
            // A tenant with users before this run that has no passkey table
            // read its passkeys from `public` (#2518).
            #[cfg(feature = "passkey")]
            let reads_public_passkeys = existed_here(&dbpool, "rustango_users").await
                && !existed_here(&dbpool, PASSKEY_TABLE).await;
            let run = async {
                let applied = migrate_with_system(
                    &dbpool,
                    chain,
                    MigrationScope::Tenant,
                    project.dirs(),
                    system_progress,
                    crate::migrate::Signals::Fire {
                        tenant: Some(&org.slug),
                    },
                    |held| {
                        let observer = project_progress
                            .as_ref()
                            .map(|o| o as &dyn crate::migrate::MigrationObserver);
                        let pool = &pool;
                        each_dir(project, move |d| {
                            crate::migrate::migrate_locked(held, pool, d, observer)
                        })
                    },
                )
                .await?;
                // Lands in the tenant schema, not `public` (#2364).
                crate::migrate::manage::ensure_tenant_bootstrap_tables(&dbpool).await?;
                Ok::<_, TenancyError>(applied)
            }
            .await;
            // Warn even if the run failed: once the tenant's passkey table exists,
            // the next run sees no change and would never warn.
            #[cfg(feature = "passkey")]
            if reads_public_passkeys && existed_here(&dbpool, PASSKEY_TABLE).await {
                warn_hidden_public_passkeys(&pools.registry_pool(), &org.slug, &schema).await;
            }
            let applied = run?;
            // Data seeders (rows, not DDL — kept): CRUD permission
            // codenames for every registered model (#61) + the
            // content-type catalog (#89). Idempotent.
            if let Err(e) = super::permissions::auto_create_permissions_pool(&dbpool).await {
                tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "auto_create_permissions_pool failed for schema-mode tenant");
            }
            if let Err(e) = crate::contenttypes::ensure_seeded(&dbpool).await {
                tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for schema-mode tenant");
            }
            // #1464 — schema mode is PG-only, so the sweep is a no-op
            // here today. Wired anyway: the invariant is "every database
            // the migrator touches gets swept", and an exception that
            // happens to be harmless is how the single-database-only
            // version of this shipped.
            normalise_datetimes_quietly(&dbpool, &org.slug).await;
            pool.close().await;
            Ok(applied)
        }
        StorageMode::Database => {
            let tenant_pool = pools.pool_for_org(org).await?;
            // System-app migrations before user migrations (issue #1171).
            let dbpool: crate::sql::Pool = tenant_pool.pool().clone().into();
            let applied = migrate_with_system(
                &dbpool,
                chain,
                MigrationScope::Tenant,
                project.dirs(),
                system_progress,
                crate::migrate::Signals::Fire {
                    tenant: Some(&org.slug),
                },
                |held| {
                    let observer = project_progress
                        .as_ref()
                        .map(|o| o as &dyn crate::migrate::MigrationObserver);
                    let pool = tenant_pool.pool();
                    each_dir(project, move |d| {
                        crate::migrate::migrate_locked(held, pool, d, observer)
                    })
                },
            )
            .await?;
            crate::migrate::manage::ensure_tenant_bootstrap_tables(&dbpool).await?;
            // Data seeders (rows, not DDL — kept): #61 + #89.
            if let Err(e) = super::permissions::auto_create_permissions_pool(&dbpool).await {
                tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "auto_create_permissions_pool failed for database-mode tenant");
            }
            if let Err(e) = crate::contenttypes::ensure_seeded(&dbpool).await {
                tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "contenttypes::ensure_seeded failed for database-mode tenant");
            }
            // #1464 — database mode on a non-PG backend is exactly the
            // deployment shape the sweep exists for.
            normalise_datetimes_quietly(&dbpool, &org.slug).await;
            Ok(applied)
        }
    }
}

/// Build a short-lived `PgPool` whose every connection has its
/// `search_path` pre-set to `<schema>, public` via an
/// `after_connect` hook. Used only for schema-mode migrations —
/// runtime requests use the shared registry pool with per-checkout
/// `SET`.
///
/// The schema is created if it doesn't exist before the migration
/// runs (so a freshly-provisioned tenant works on its first
/// `migrate_tenants` call). One that holds tables but no ledger is
/// adopted with a warning; a later release refuses it.
#[cfg(feature = "postgres")]
async fn build_schema_scoped_pool(
    registry_url: &str,
    schema: &str,
) -> Result<PgPool, TenancyError> {
    // Ensure the schema exists. Use a one-shot connection so we don't
    // pollute the migration pool's connections.
    let bootstrap = PgPool::connect(registry_url).await?;
    let origin = ensure_schema(&bootstrap, schema).await;
    bootstrap.close().await;
    if origin? == SchemaOrigin::Foreign {
        return Err(TenancyError::Validation(format!(
            "schema `{schema}` already holds tables but no rustango ledger, so another app \
             may own it; move the tenant to a schema of its own"
        )));
    }

    super::pools::connect_schema_scoped(registry_url, schema, 2).await
}

/// Where a tenant's schema came from, by [`ensure_schema`].
#[cfg(feature = "postgres")]
#[derive(Debug, PartialEq, Eq)]
enum SchemaOrigin {
    Created,
    /// Empty, or it has a rustango ledger.
    Kept,
    /// Tables, but no ledger: something else made it.
    Foreign,
}

/// `CREATE SCHEMA`, and on a clash, whether the existing one looks like
/// the tenant's. Catalog SQL: schema mode is Postgres-only.
#[cfg(feature = "postgres")]
async fn ensure_schema(pg: &PgPool, schema: &str) -> Result<SchemaOrigin, sqlx::Error> {
    use crate::migrate::{LEDGER_TABLE, SYSTEM_LEDGER_TABLE};
    let create = format!("CREATE SCHEMA {}", crate::sql::quote_with(schema, '"'));
    match rustango::sql::sqlx::query(&create).execute(pg).await {
        Ok(_) => return Ok(SchemaOrigin::Created),
        // duplicate_schema, or a concurrent create's unique_violation.
        Err(sqlx::Error::Database(e))
            if e.code().as_deref() == Some("42P06")
                || (e.code().as_deref() == Some("23505")
                    && e.constraint() == Some("pg_namespace_nspname_index")) => {}
        Err(e) => return Err(e),
    }
    let (tables, ledgers): (i64, i64) = rustango::sql::sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE c.relname IN ($2, $3)) \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
    )
    .bind(schema)
    .bind(LEDGER_TABLE)
    .bind(SYSTEM_LEDGER_TABLE)
    .fetch_one(pg)
    .await?;
    Ok(if tables > 0 && ledgers == 0 {
        SchemaOrigin::Foreign
    } else {
        SchemaOrigin::Kept
    })
}

/// Prep a directory of migrations filtered by scope. If every
/// migration in the input dir already matches `scope`, returns
/// [`ScopedDir::Original`] (no copy). Otherwise materializes a
/// temp dir containing only the matching files and returns
/// [`ScopedDir::Owned`].
pub(crate) async fn scoped_subset(
    dir: &Path,
    scope: MigrationScope,
) -> Result<ScopedDir, TenancyError> {
    let all = rustango::migrate::file::list_dir(dir)?;
    if all.iter().all(|m| m.scope == scope) {
        return Ok(ScopedDir::Original);
    }
    Ok(ScopedDir::Owned(scoped_copy(dir, scope)?))
}

/// `dir`'s `scope` migrations copied into a new temp dir, even when that is all of them.
pub(crate) fn scoped_copy(
    dir: &Path,
    scope: MigrationScope,
) -> Result<tempfile::TempDir, TenancyError> {
    let temp = scoped_tempdir()?;
    for mig in rustango::migrate::file::list_dir(dir)? {
        if mig.scope == scope {
            let target = temp.path().join(format!("{}.json", mig.name));
            rustango::migrate::file::write(&target, &mig)?;
        }
    }
    Ok(temp)
}

pub(crate) enum ScopedDir {
    /// All migrations in `dir` already match the requested scope —
    /// run directly against the original directory.
    Original,
    /// Materialized a temp dir holding only the matching files.
    /// Caller drops to clean up.
    Owned(tempfile::TempDir),
}

impl ScopedDir {
    /// The directory to run: the filtered copy, or `original` itself.
    pub(crate) fn path<'a>(&'a self, original: &'a Path) -> &'a Path {
        match self {
            Self::Owned(temp) => temp.path(),
            Self::Original => original,
        }
    }
}

/// A new dir with a random name, owner-only on Unix: a leftover or planted
/// dir is never reused, so only this run's files are applied.
fn scoped_tempdir() -> Result<tempfile::TempDir, TenancyError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("rustango_tenancy_scoped_");
    // tempfile makes dirs (unlike files) with the default 0777 less the umask.
    #[cfg(unix)]
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
    builder.tempdir().map_err(|e| {
        TenancyError::Validation(format!("could not create scoped-migration tempdir: {e}"))
    })
}

#[cfg(all(test, feature = "postgres"))]
mod schema_pool_tests {
    use super::build_schema_scoped_pool;

    /// Schema pools must not share a cache scope with each other or the registry.
    #[tokio::test]
    async fn schema_scoped_pools_have_their_own_scope_key() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pid = std::process::id();
        let (a, b) = (format!("scope \"a\" {pid}"), format!("scope_b_{pid}"));
        let registry = crate::sql::Pool::connect(&url).await.expect("registry");
        let pa = crate::sql::Pool::Postgres(build_schema_scoped_pool(&url, &a).await.expect("a"));
        let pb = crate::sql::Pool::Postgres(build_schema_scoped_pool(&url, &b).await.expect("b"));
        let current: (String,) = sqlx::query_as("SELECT current_schema()::text")
            .fetch_one(pa.as_postgres().expect("pg"))
            .await
            .expect("current_schema");
        assert_eq!(current.0, a, "search_path not applied");
        let keys = [registry.scope_key(), pa.scope_key(), pb.scope_key()];
        assert_eq!(pa.clone().scope_key(), keys[1], "a clone keeps its scope");
        pa.close().await;
        pb.close().await;
        for s in [&a, &b] {
            let drop = format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                crate::sql::quote_with(s, '"')
            );
            let _ = sqlx::query(&drop)
                .execute(registry.as_postgres().expect("pg"))
                .await;
        }
        assert_ne!(keys[0], keys[1], "schema pool shares the registry's scope");
        assert_ne!(keys[1], keys[2], "two schema pools share a scope");
    }

    /// An existing schema with tables but no ledger was not made for the
    /// tenant: migrating would adopt it, so it is told apart.
    #[tokio::test]
    async fn a_foreign_schema_is_told_from_the_tenants_own() {
        use super::{ensure_schema, SchemaOrigin};
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pg = crate::sql::sqlx::PgPool::connect(&url)
            .await
            .expect("registry");
        let s = format!("origin_{}", std::process::id());
        let q = crate::sql::quote_with(&s, '"');
        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {q} CASCADE"))
            .execute(&pg)
            .await;
        assert_eq!(ensure_schema(&pg, &s).await.unwrap(), SchemaOrigin::Created);
        assert_eq!(ensure_schema(&pg, &s).await.unwrap(), SchemaOrigin::Kept);
        sqlx::query(&format!("CREATE TABLE {q}.app_data (id INT)"))
            .execute(&pg)
            .await
            .unwrap();
        assert_eq!(ensure_schema(&pg, &s).await.unwrap(), SchemaOrigin::Foreign);
        // Migrating into it is refused, not adopted.
        let refused = super::build_schema_scoped_pool(&url, &s).await;
        assert!(
            matches!(refused, Err(crate::tenancy::TenancyError::Validation(_))),
            "{refused:?}"
        );
        let ledger = crate::migrate::LEDGER_TABLE;
        sqlx::query(&format!("CREATE TABLE {q}.\"{ledger}\" (name TEXT)"))
            .execute(&pg)
            .await
            .unwrap();
        assert_eq!(ensure_schema(&pg, &s).await.unwrap(), SchemaOrigin::Kept);
        let _ = sqlx::query(&format!("DROP SCHEMA {q} CASCADE"))
            .execute(&pg)
            .await;
    }
}

#[cfg(test)]
mod scoped_subset_tests {
    use super::{scoped_subset, MigrationScope};

    fn write(dir: &std::path::Path, name: &str, scope: MigrationScope) {
        let mig = crate::migrate::Migration {
            name: name.to_owned(),
            created_at: "2026-10-29T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope,
            replaces: Vec::new(),
            merges: Vec::new(),
            snapshot: serde_json::from_value(serde_json::json!({ "tables": [] })).unwrap(),
            forward: Vec::new(),
        };
        crate::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();
    }

    /// A dir left or planted under a guessable name is never reused; the
    /// copy is owner-only and gone once dropped.
    #[tokio::test]
    async fn the_scoped_copy_is_fresh_and_private() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "0001_reg", MigrationScope::Registry);
        write(src.path(), "0002_ten", MigrationScope::Tenant);
        let pid = std::process::id();
        let planted: Vec<_> = (0..256)
            .map(|n| std::env::temp_dir().join(format!("rustango_tenancy_scoped_{pid}_{n}")))
            .collect();
        for p in &planted {
            std::fs::create_dir_all(p).unwrap();
            write(p, "0099_planted", MigrationScope::Registry);
        }
        let scoped = scoped_subset(src.path(), MigrationScope::Registry).await;
        for p in &planted {
            let _ = std::fs::remove_dir_all(p);
        }
        let scoped = scoped.expect("scoped copy");
        let path = scoped.path(src.path()).to_path_buf();
        let names: Vec<String> = crate::migrate::file::list_dir(&path)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["0001_reg"], "{}", path.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        }
        drop(scoped);
        assert!(!path.exists(), "left behind: {}", path.display());
    }
}
