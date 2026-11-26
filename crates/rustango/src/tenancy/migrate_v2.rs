//! Engine v2 under tenancy (8.1-8.4): the registry chain once, on the
//! registry database, then each tenant's chain on its schema (PG) or its
//! database, under one fleet lock and record.

use std::path::Path;

use sqlx::Database;

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::pools::TenantPools;
use crate::migrate::v2::run::{self, Options, RunAs};
use crate::migrate::LockScope;

/// One tenant's v2 run: its pool, its lock and its login role.
pub(crate) struct Target {
    pub(crate) slug: String,
    pub(crate) pool: crate::sql::Pool,
    pub(crate) scope: LockScope,
    /// The tenant's own login, which gets no write access to the ledger.
    pub(crate) role: Option<String>,
    /// The fleet made this pool (schema mode) and closes it after.
    owned: bool,
}

/// The login the fleet migrates database-mode tenants as, from
/// `RUSTANGO_TENANT_MIGRATOR=user:password`. When it differs from a
/// tenant's own login, that login gets no write access to the ledger (8.1).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MigratorLogin {
    pub(crate) user: String,
    pub(crate) password: String,
}

impl std::fmt::Debug for MigratorLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigratorLogin")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl MigratorLogin {
    pub(crate) const ENV: &'static str = "RUSTANGO_TENANT_MIGRATOR";

    /// [`Self::ENV`], when set.
    pub(crate) fn from_env() -> Option<Self> {
        let v = std::env::var(Self::ENV).ok()?;
        let (user, password) = v.split_once(':')?;
        Some(Self {
            user: user.to_owned(),
            password: password.to_owned(),
        })
    }
}

/// `org`'s v2 target, migrated as its own login.
///
/// # Errors
/// As [`target_as`].
#[cfg(test)]
pub(crate) async fn target<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    registry_url: &str,
) -> Result<Target, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    target_as(pools, org, registry_url, None).await
}

/// `org`'s v2 target. A schema tenant gets a pool whose path is its schema
/// alone: nothing falls through to `public` (decision 1). A database tenant
/// is migrated as `migrator` when given and not its own login.
///
/// # Errors
/// An unreachable database, a foreign schema, or schema mode off PG.
pub(crate) async fn target_as<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    registry_url: &str,
    migrator: Option<&MigratorLogin>,
) -> Result<Target, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if org.storage_mode == StorageMode::Schema {
        #[cfg(feature = "postgres")]
        {
            let pg = (pools as &dyn std::any::Any)
                .downcast_ref::<TenantPools<sqlx::Postgres>>()
                .ok_or_else(|| {
                    TenancyError::Validation(format!(
                        "org `{}` is schema-mode but the registry is not Postgres",
                        org.slug
                    ))
                })?;
            let schema = org.effective_schema().to_owned();
            let timeout = pg.pool_config().database_pool_acquire_timeout;
            super::migrate::ensure_tenant_schema(registry_url, &schema, timeout).await?;
            // The schema, then schemas holding only extension objects (8.2).
            let path = super::pools::probe_path(
                registry_url,
                &schema,
                super::pools::FallThrough::V2Unit,
                timeout,
            )
            .await?;
            let pool = schema_only_pool(registry_url, &path, timeout).await?;
            let role = pg.ensure_tenant_role(&schema).await?;
            return Ok(Target {
                slug: org.slug.clone(),
                pool: pool.into(),
                scope: LockScope::Schema(schema),
                role,
                owned: true,
            });
        }
        #[cfg(not(feature = "postgres"))]
        return Err(TenancyError::Validation(format!(
            "org `{}` is schema-mode, which needs Postgres",
            org.slug
        )));
    }
    let _ = registry_url;
    if let Some(m) = migrator {
        let url = pools.resolved_database_url(org).await?;
        if let Some((pool, tenant_login)) = as_migrator(&url, m).await? {
            return Ok(Target {
                slug: org.slug.clone(),
                pool,
                scope: LockScope::Session,
                role: Some(tenant_login),
                owned: true,
            });
        }
    }
    let pool = match pools.database_pool_for_org(org).await? {
        super::pools::TenantPool::Database { pool } => crate::sql::Pool::from((*pool).clone()),
        #[cfg(feature = "postgres")]
        super::pools::TenantPool::Schema { .. } => {
            unreachable!("database_pool_for_org refuses schema mode")
        }
    };
    Ok(Target {
        slug: org.slug.clone(),
        pool,
        scope: LockScope::Session,
        role: None,
        owned: false,
    })
}

/// A pool on the tenant database at `url` logged in as `m`, and the
/// tenant's own login; `None` when they are the same login (the
/// documented gap) or the backend has no logins.
#[allow(clippy::unused_async)]
async fn as_migrator(
    url: &str,
    m: &MigratorLogin,
) -> Result<Option<(crate::sql::Pool, String)>, TenancyError> {
    let _ = m;
    match super::BackendKind::of_url(url) {
        #[cfg(feature = "mysql")]
        Some(super::BackendKind::MySql) => {
            let opts: sqlx::mysql::MySqlConnectOptions = url.parse()?;
            let tenant = opts.get_username().to_owned();
            if tenant == m.user {
                return Ok(None);
            }
            let pool = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(2)
                .connect_with(opts.username(&m.user).password(&m.password))
                .await?;
            Ok(Some((pool.into(), tenant)))
        }
        #[cfg(feature = "postgres")]
        Some(super::BackendKind::Postgres) => {
            let opts: sqlx::postgres::PgConnectOptions = url.parse()?;
            let tenant = opts.get_username().to_owned();
            if tenant == m.user {
                return Ok(None);
            }
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect_with(opts.username(&m.user).password(&m.password))
                .await?;
            Ok(Some((pool.into(), tenant)))
        }
        _ => Ok(None),
    }
}

/// A small pool on the registry database whose every connection's path is
/// `path`: the tenant schema and extension-only schemas, never registry tables.
#[cfg(feature = "postgres")]
async fn schema_only_pool(
    registry_url: &str,
    path: &super::pools::TenantSearchPath,
    timeout: std::time::Duration,
) -> Result<sqlx::PgPool, TenancyError> {
    let path = path.set_sql();
    Ok(sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(timeout)
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                use sqlx::Executor as _;
                conn.execute(sqlx::raw_sql(&path)).await.map(drop)
            })
        })
        .connect(registry_url)
        .await?)
}

/// How a fleet run goes (8.3): `migrate-tenants` flags, or boot's
/// `[migrate]` settings.
#[derive(Debug, Clone, Default)]
pub(crate) struct FleetOptions {
    /// Stop after more than this many tenants failed; `None` runs them all.
    /// `--fail-fast` is `Some(0)`.
    pub(crate) max_failures: Option<usize>,
    /// Tenants migrated at once after the canaries; `0` or `1` is one by one.
    pub(crate) parallel: usize,
    /// Slugs migrated first, one by one; a canary failure stops the fleet.
    pub(crate) canaries: Vec<String>,
    /// Inactive tenants are left out (`--active-only`); by default every
    /// tenant is migrated (decision 10).
    pub(crate) active_only: bool,
    /// `--phase expand`: stop every tenant before its first contract file.
    pub(crate) expand_only: bool,
    /// `--allow-mixed-heads`: no expand gate before contract files.
    pub(crate) allow_mixed_heads: bool,
    /// `--abandon-fleet-run`: a run with another plan that did not finish
    /// is marked abandoned instead of refusing this one.
    pub(crate) abandon: bool,
    /// The login database tenants are migrated as ([`MigratorLogin::from_env`]).
    pub(crate) migrator: Option<MigratorLogin>,
}

/// One tenant's result.
#[derive(Debug, Clone)]
pub(crate) struct TenantOutcome {
    pub(crate) slug: String,
    pub(crate) applied: Vec<String>,
    /// The error class (as the registry cache holds it).
    pub(crate) error_class: Option<String>,
    /// The full error, for the operator's log only.
    pub(crate) error: Option<String>,
}

/// What a fleet run did.
#[derive(Debug, Clone, Default)]
pub(crate) struct FleetReport {
    pub(crate) registry: Vec<String>,
    pub(crate) tenants: Vec<TenantOutcome>,
    /// `--max-failures` or a canary stopped the run; later tenants did not run.
    pub(crate) stopped: bool,
    /// Tenants a stop left unmigrated; boot parks them with the failed ones.
    pub(crate) skipped: Vec<String>,
    /// The fleet record of this run (8.3).
    pub(crate) run_id: String,
}

impl FleetReport {
    /// The tenants that failed.
    pub(crate) fn failed(&self) -> impl Iterator<Item = &TenantOutcome> {
        self.tenants.iter().filter(|t| t.error_class.is_some())
    }
}

fn migrate_err(e: run::RunError) -> TenancyError {
    TenancyError::Migrate(e.into())
}

/// The registry chain, then every tenant's, under the fleet lock and a
/// fleet record (8.3). A registry failure is an error; a tenant failure is
/// recorded in the registry cache (its error class only) and the run goes
/// on, within `max_failures`. Every tenant is planned first: one blocker
/// refuses them all before any change. Canaries go first; every tenant
/// reaches its expand head before any contract file runs; tenants added
/// while it runs are picked up before it ends.
///
/// # Errors
/// The registry chain, a blocker in any tenant, another unfinished fleet
/// plan, or reading the orgs.
pub(crate) async fn run_fleet<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
    fo: &FleetOptions,
) -> Result<FleetReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let result: Result<Result<FleetReport, TenancyError>, crate::migrate::MigrateError> =
        run::fleet_locked(&registry, || async {
            Ok(fleet_body(pools, &registry, dir, registry_url, fo).await)
        })
        .await;
    result?
}

/// The fleet's tenants: every org (or the active ones), by slug order.
async fn fleet_orgs(
    registry: &crate::sql::Pool,
    fo: &FleetOptions,
) -> Result<Vec<Org>, TenancyError> {
    use crate::core::{Column as _, Model as _};
    // A fresh registry has no orgs table until its chain runs.
    if !crate::migrate::try_table_exists_here(registry, Org::SCHEMA.table).await? {
        return Ok(Vec::new());
    }
    let qs = if fo.active_only {
        Org::objects().where_(Org::active.eq(true))
    } else {
        Org::objects()
    };
    let (orgs, unreadable) = Org::fetch_reporting(qs.order_by(&[("slug", false)]), registry)
        .await
        .map_err(crate::migrate::MigrateError::from)?;
    if !unreadable.is_empty() {
        tracing::warn!(
            target: "rustango::tenancy",
            unreadable = unreadable.len(),
            "org rows the fleet could not read are not migrated"
        );
    }
    Ok(orgs)
}

/// A tenant run's options.
fn tenant_opts(t: &Target) -> Options {
    Options {
        run_as: RunAs::Tenant,
        lock_scope: t.scope.clone(),
        ..Options::default()
    }
}

async fn fleet_body<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    dir: &Path,
    registry_url: &str,
    fo: &FleetOptions,
) -> Result<FleetReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut report = FleetReport::default();
    #[cfg(feature = "postgres")]
    if registry.dialect().kind() == crate::sql::DialectKind::Postgres {
        extensions_home(registry).await?;
    }
    let orgs = fleet_orgs(registry, fo).await?;
    let (targets, unreachable) = targets_of(pools, &orgs, registry_url, fo.migrator.as_ref()).await;
    report.tenants.extend(unreachable);
    // `requires: tenants:<id>` holds when every tenant has applied it.
    let mut everywhere: Option<std::collections::BTreeSet<String>> = None;
    for t in &targets {
        let ids = run::applied_ids(&t.pool, &tenant_opts(t))
            .await
            .map_err(migrate_err)?;
        everywhere = Some(match everywhere {
            None => ids,
            Some(all) => all.intersection(&ids).cloned().collect(),
        });
    }
    let reg = Options {
        run_as: RunAs::Registry,
        tenants_applied: Some(everywhere.unwrap_or_default()),
        ..Options::default()
    };
    report.registry = run::migrate(registry, dir, &reg)
        .await
        .map_err(migrate_err)?
        .applied;
    let registry_applied = run::applied_ids(registry, &reg)
        .await
        .map_err(migrate_err)?;
    #[cfg(feature = "postgres")]
    if registry.dialect().kind() == crate::sql::DialectKind::Postgres
        && orgs.iter().any(|o| o.storage_mode == StorageMode::Schema)
    {
        refuse_missing_extensions(
            registry,
            &run::registry_extensions(dir).map_err(migrate_err)?,
        )
        .await?;
    }
    let plan_hash = run::plan_hash(dir).map_err(migrate_err)?;
    report.run_id = run::new_run_id();
    crate::migrate::v2::ledger::fleet_open(registry, &report.run_id, "up", &plan_hash, fo.abandon)
        .await?;
    crate::migrate::v2::ledger::cache_ensure(registry).await?;
    let run_id = report.run_id.clone();
    // One tip for every tenant: each distinct applied set plans once (4.4).
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let plan = crate::migrate::v2::plan::TipPlanner::new(
        &project,
        crate::migrate::v2::state::Target::Tenant,
    )
    .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let fleet = Fleet {
        registry,
        dir,
        registry_applied: &registry_applied,
        run_id: &run_id,
        plan: std::sync::Arc::new(plan),
    };
    let seen = orgs.iter().map(|o| o.slug.clone()).collect();
    let result = fleet_rounds(pools, &fleet, registry_url, fo, targets, seen, &mut report).await;
    let state = if result.is_err() || report.failed().next().is_some() || report.stopped {
        "failed"
    } else {
        "succeeded"
    };
    crate::migrate::v2::ledger::fleet_close(registry, &run_id, state).await?;
    result.map(|()| report)
}

/// The fleet's rounds: the tenants known, then those provisioned meanwhile.
#[allow(clippy::too_many_lines)]
async fn fleet_rounds<DB: Database>(
    pools: &TenantPools<DB>,
    fleet: &Fleet<'_>,
    registry_url: &str,
    fo: &FleetOptions,
    mut targets: Vec<Target>,
    mut seen: std::collections::BTreeSet<String>,
    report: &mut FleetReport,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = fleet.registry;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    // Shared by the buffered tenant futures; atomic so the fleet future is `Send`.
    let failures = AtomicUsize::new(report.tenants.len());
    let bump = |n: usize| failures.fetch_add(n, SeqCst);
    loop {
        fleet.refuse_blockers(&targets).await?;
        // Canaries first, one by one, then the rest; contract files only
        // once every tenant reached its expand head.
        let gated = !fo.allow_mixed_heads && fleet.any_contract_pending(&targets).await;
        let stages: &[bool] = if fo.expand_only || gated {
            if fo.expand_only {
                &[true]
            } else {
                &[true, false]
            }
        } else {
            &[false]
        };
        let (canaries, rest): (Vec<&Target>, Vec<&Target>) =
            targets.iter().partition(|t| fo.canaries.contains(&t.slug));
        for &expand in stages {
            let before = failures.load(SeqCst);
            for (n, t) in canaries.iter().enumerate() {
                let out = fleet.migrate_one(t, expand).await?;
                let failed = out.error_class.is_some();
                if failed {
                    bump(1);
                }
                report.tenants.push(out);
                if failed {
                    tracing::error!(
                        target: "rustango::tenancy",
                        slug = %t.slug,
                        "a canary failed: the fleet stops"
                    );
                    report.stopped = true;
                    report.skipped.extend(
                        canaries[n + 1..]
                            .iter()
                            .chain(&rest)
                            .map(|t| t.slug.clone()),
                    );
                    break;
                }
            }
            if report.stopped {
                break;
            }
            use futures_util::StreamExt as _;
            let k = fo.parallel.max(1);
            let mut stream = futures_util::stream::iter(rest.iter().map(|t| async {
                if fo.max_failures.is_some_and(|m| failures.load(SeqCst) > m) {
                    return Ok(Err(t.slug.clone()));
                }
                fleet.migrate_one(t, expand).await.map(Ok)
            }))
            .buffered(k);
            while let Some(out) = stream.next().await {
                match out? {
                    Ok(o) => {
                        if o.error_class.is_some() {
                            bump(1);
                        }
                        report.tenants.push(o);
                    }
                    Err(slug) => {
                        report.stopped = true;
                        report.skipped.push(slug);
                    }
                }
            }
            drop(stream);
            if fo.max_failures.is_some_and(|m| failures.load(SeqCst) > m) {
                report.stopped = true;
            }
            // A tenant short of its expand head keeps contract files off all.
            if expand && failures.load(SeqCst) > before {
                tracing::error!(
                    target: "rustango::tenancy",
                    "a tenant did not reach its expand head: no contract file runs this time"
                );
                break;
            }
            if report.stopped {
                break;
            }
        }
        crate::migrate::v2::ledger::fleet_beat(registry, fleet.run_id).await?;
        for t in std::mem::take(&mut targets).into_iter().filter(|t| t.owned) {
            t.pool.close().await;
        }
        if report.stopped {
            break;
        }
        // Tenants provisioned while the fleet ran: migrate them too, until none is new.
        let fresh: Vec<Org> = fleet_orgs(registry, fo)
            .await?
            .into_iter()
            .filter(|o| seen.insert(o.slug.clone()))
            .collect();
        if fresh.is_empty() {
            break;
        }
        let (t, unreachable) = targets_of(pools, &fresh, registry_url, fo.migrator.as_ref()).await;
        bump(unreachable.len());
        report.tenants.extend(unreachable);
        targets = t;
    }
    Ok(())
}

/// The schema extensions live in (8.2).
#[cfg(feature = "postgres")]
const EXTENSIONS: &str = crate::migrate::v2::render::EXTENSIONS_SCHEMA;

/// The one-time extension move (8.2): relocatable extensions in `public`
/// go to `extensions`, and the database's path gains it, so registry
/// tables stay off tenant paths and citext keeps its operators.
///
/// # Errors
/// Missing rights: the error names the statement to run as the owner.
#[cfg(feature = "postgres")]
async fn extensions_home(registry: &crate::sql::Pool) -> Result<(), TenancyError> {
    use crate::sql::raw_query_pool;
    let q = |sql: &'static str| async move {
        raw_query_pool::<(String,)>(sql, Vec::new(), registry)
            .await
            .map_err(crate::migrate::MigrateError::from)
    };
    let movable = q("SELECT e.extname::text FROM pg_catalog.pg_extension e \
         JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
         WHERE n.nspname = 'public' AND e.extrelocatable ORDER BY 1")
    .await?;
    let homed = q("SELECT e.extname::text FROM pg_catalog.pg_extension e \
         JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
         WHERE n.nspname = 'extensions'")
    .await?;
    // Nothing to move and nothing homed: no rights needed, nothing runs.
    if movable.is_empty() && homed.is_empty() {
        return Ok(());
    }
    let path_set = q(
        "SELECT d.datname::text FROM pg_catalog.pg_db_role_setting s \
         JOIN pg_catalog.pg_database d ON d.oid = s.setdatabase \
         WHERE d.datname = current_database() AND s.setrole = 0 \
         AND EXISTS (SELECT 1 FROM unnest(s.setconfig) c \
                     WHERE c LIKE 'search_path=%extensions%')",
    )
    .await?;
    if movable.is_empty() && !path_set.is_empty() {
        return Ok(());
    }
    let db = q("SELECT current_database()::text").await?;
    let db = crate::sql::quote_with(&db[0].0, '"');
    let mut stmts = vec![format!("CREATE SCHEMA IF NOT EXISTS {EXTENSIONS}")];
    stmts.extend(movable.iter().map(|(e,)| {
        format!(
            "ALTER EXTENSION {} SET SCHEMA {EXTENSIONS}",
            crate::sql::quote_with(e, '"')
        )
    }));
    if path_set.is_empty() {
        stmts.push(format!(
            "ALTER DATABASE {db} SET search_path TO \"$user\", public, {EXTENSIONS}"
        ));
    }
    for sql in &stmts {
        if let Err(e) = crate::sql::raw_execute_pool(registry, sql, Vec::new()).await {
            return Err(TenancyError::Validation(format!(
                "the extension move needs the extension owner or a superuser: run `{sql}` as \
                 one, then migrate again ({e})"
            )));
        }
        tracing::info!(target: "rustango::tenancy", "{sql}");
    }
    Ok(())
}

/// `open` before the first tenant statement (8.2): every extension the
/// registry chain declares is installed and on the tenant path.
///
/// # Errors
/// A missing extension, or one in a schema that stays off tenant paths.
#[cfg(feature = "postgres")]
async fn refuse_missing_extensions(
    registry: &crate::sql::Pool,
    needed: &std::collections::BTreeSet<String>,
) -> Result<(), TenancyError> {
    if needed.is_empty() {
        return Ok(());
    }
    let installed: Vec<(String, String)> = crate::sql::raw_query_pool(
        "SELECT e.extname::text, n.nspname::text FROM pg_catalog.pg_extension e \
         JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace",
        Vec::new(),
        registry,
    )
    .await
    .map_err(crate::migrate::MigrateError::from)?;
    let mut wrong = Vec::new();
    for name in needed {
        match installed.iter().find(|(e, _)| e == name) {
            None => wrong.push(format!(
                "`{name}` is not installed: run `CREATE EXTENSION {name} SCHEMA {EXTENSIONS}`"
            )),
            Some((_, schema)) if schema != EXTENSIONS && schema != "pg_catalog" => {
                wrong.push(format!(
                    "`{name}` is in schema `{schema}`, which tenant paths leave out: run \
                     `ALTER EXTENSION {name} SET SCHEMA {EXTENSIONS}`"
                ));
            }
            Some(_) => {}
        }
    }
    if wrong.is_empty() {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "no tenant was changed; extensions the registry chain declares: {}",
        wrong.join("; ")
    )))
}

/// Under tenancy, who still needs the files squash checkpoint `id`
/// replaces (9.6): the registry, or each tenant (inactive ones too), that
/// ran part of them and not the checkpoint. An unreachable tenant counts.
///
/// # Errors
/// No such checkpoint, a load error, or reading a ledger.
pub(crate) async fn prune_gate<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
    id: &str,
) -> Result<crate::migrate::v2::squash::PruneGate, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::migrate::v2::squash::{needs_replaced, squash_checkpoint, PruneGate};
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let c = squash_checkpoint(&project, id)
        .ok_or_else(|| TenancyError::Validation(format!("no squash checkpoint `{id}`")))?
        .file
        .clone();
    let ledger = crate::migrate::v2::ledger::Ledger::default_names();
    let applied =
        |rows: Vec<crate::migrate::v2::ledger::Row>| -> std::collections::HashSet<String> {
            rows.into_iter()
                .filter(|r| {
                    r.app == c.app && r.state == crate::migrate::v2::ledger::RowState::Applied
                })
                .map(|r| r.id)
                .collect()
        };
    let registry = pools.registry_pool();
    let mut behind = Vec::new();
    if c.target == crate::migrate::v2::state::Target::Registry {
        if needs_replaced(&c, &applied(ledger.rows(&registry).await?)) {
            behind.push("the registry".to_owned());
        }
        return Ok(PruneGate::Fleet { behind });
    }
    let orgs = fleet_orgs(&registry, &FleetOptions::default()).await?;
    let (targets, unreachable) = targets_of(pools, &orgs, registry_url, None).await;
    behind.extend(
        unreachable
            .into_iter()
            .map(|t| format!("{} (unreachable)", t.slug)),
    );
    for t in targets {
        if needs_replaced(&c, &applied(ledger.rows(&t.pool).await?)) {
            behind.push(t.slug.clone());
        }
        if t.owned {
            t.pool.close().await;
        }
    }
    Ok(PruneGate::Fleet { behind })
}

/// Each org's target, and an outcome for each one unreachable.
async fn targets_of<DB: Database>(
    pools: &TenantPools<DB>,
    orgs: &[Org],
    registry_url: &str,
    migrator: Option<&MigratorLogin>,
) -> (Vec<Target>, Vec<TenantOutcome>)
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut targets = Vec::new();
    let mut unreachable = Vec::new();
    for org in orgs {
        match target_as(pools, org, registry_url, migrator).await {
            Ok(t) => targets.push(t),
            Err(e) => unreachable.push(TenantOutcome {
                slug: org.slug.clone(),
                applied: Vec::new(),
                error_class: Some("unreachable".into()),
                error: Some(e.to_string()),
            }),
        }
    }
    (targets, unreachable)
}

/// What every tenant run of one fleet run shares.
struct Fleet<'a> {
    registry: &'a crate::sql::Pool,
    dir: &'a Path,
    registry_applied: &'a std::collections::BTreeSet<String>,
    run_id: &'a str,
    plan: std::sync::Arc<crate::migrate::v2::plan::TipPlanner>,
}

impl Fleet<'_> {
    fn opts(&self, t: &Target, dry_run: bool, expand: bool) -> Options {
        Options {
            registry_applied: Some(self.registry_applied.clone()),
            revoke_from: t.role.clone(),
            dry_run,
            stop_before_contract: expand,
            plan: Some(self.plan.clone()),
            fast_fresh: true,
            ..tenant_opts(t)
        }
    }

    /// Plan every tenant before any change: a refusal anywhere stops all.
    async fn refuse_blockers(&self, targets: &[Target]) -> Result<(), TenancyError> {
        let mut blockers = Vec::new();
        for t in targets {
            if let Some(login) = &t.role {
                if let Some(wide) = crate::migrate::v2::ledger::wide_grants(&t.pool, login).await? {
                    blockers.push(format!("  - {}: {wide}", t.slug));
                    continue;
                }
            }
            if let Err(e) = run::migrate(&t.pool, self.dir, &self.opts(t, true, false)).await {
                if e.code().is_some() {
                    blockers.push(format!("  - {}: {e}", t.slug));
                }
            }
        }
        if blockers.is_empty() {
            return Ok(());
        }
        Err(TenancyError::Validation(format!(
            "the fleet is blocked; no tenant was changed:\n{}",
            blockers.join("\n")
        )))
    }

    /// Whether any tenant has a contract file pending.
    async fn any_contract_pending(&self, targets: &[Target]) -> bool {
        for t in targets {
            if run::contract_pending(&t.pool, self.dir, &tenant_opts(t))
                .await
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }

    /// One tenant's run, recorded in the registry cache.
    async fn migrate_one(&self, t: &Target, expand: bool) -> Result<TenantOutcome, TenancyError> {
        use crate::migrate::v2::ledger::cache_put;
        cache_put(self.registry, &t.slug, "running", None, None, self.run_id).await?;
        Ok(
            match run::migrate(&t.pool, self.dir, &self.opts(t, false, expand)).await {
                Ok(rep) => {
                    let head = run::applied_ids(&t.pool, &tenant_opts(t))
                        .await
                        .ok()
                        .and_then(|ids| ids.into_iter().filter(|i| !i.contains(':')).max());
                    cache_put(
                        self.registry,
                        &t.slug,
                        "applied",
                        head.as_deref(),
                        None,
                        self.run_id,
                    )
                    .await?;
                    TenantOutcome {
                        slug: t.slug.clone(),
                        applied: rep.applied,
                        error_class: None,
                        error: None,
                    }
                }
                Err(e) => {
                    let class = run::error_class(&e);
                    cache_put(
                        self.registry,
                        &t.slug,
                        "failed",
                        None,
                        Some(class),
                        self.run_id,
                    )
                    .await?;
                    tracing::warn!(
                        target: "rustango::tenancy",
                        slug = %t.slug,
                        error = %e,
                        "tenant migration failed; continuing with the rest"
                    );
                    TenantOutcome {
                        slug: t.slug.clone(),
                        applied: Vec::new(),
                        error_class: Some(class.to_owned()),
                        error: Some(e.to_string()),
                    }
                }
            },
        )
    }
}

#[cfg(test)]
#[path = "migrate_v2_tests.rs"]
mod tests;
