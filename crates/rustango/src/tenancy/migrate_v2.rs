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

/// One tenant's v2 run: how to reach it, its lock and its login role. No
/// connection is held: each step opens one and closes it after (8.3).
pub(crate) struct Target {
    pub(crate) slug: String,
    pub(crate) scope: LockScope,
    /// The tenant's own login, which gets no write access to the ledger.
    pub(crate) role: Option<String>,
    conn: Conn,
}

/// How a [`Target`] reaches its tenant.
enum Conn {
    /// The tenant schema on the registry database, its path pinned.
    #[cfg(feature = "postgres")]
    Schema {
        url: String,
        path: super::pools::TenantSearchPath,
        timeout: std::time::Duration,
    },
    /// The tenant database as the migrator login.
    Migrator { url: String, login: MigratorLogin },
    /// The tenant database as its own login, never through the app's cached
    /// pool: the fleet's connections follow `--parallel` (8.3).
    Direct {
        url: String,
        timeout: std::time::Duration,
    },
}

/// A target's pool while one step uses it.
pub(crate) struct Open {
    pub(crate) pool: crate::sql::Pool,
}

impl Open {
    /// Release the connections the fleet opened for this step.
    pub(crate) async fn close(self) {
        self.pool.close().await;
    }
}

impl Target {
    /// A database-mode tenant: its database is not the registry's.
    pub(crate) fn own_database(&self) -> bool {
        !matches!(self.scope, LockScope::Schema(_))
    }

    /// A pool for one step; [`Open::close`] after.
    ///
    /// # Errors
    /// An unreachable database or a refused login.
    pub(crate) async fn open(&self) -> Result<Open, TenancyError> {
        let pool = match &self.conn {
            #[cfg(feature = "postgres")]
            Conn::Schema { url, path, timeout } => {
                schema_only_pool(url, path, *timeout).await?.into()
            }
            Conn::Migrator { url, login } => connect_step(url, Some(login), None).await?,
            Conn::Direct { url, timeout } => connect_step(url, None, Some(*timeout)).await?,
        };
        Ok(Open { pool })
    }

    /// Whether the fleet logs in as the migrator, not the tenant.
    fn as_migrator(&self) -> bool {
        matches!(self.conn, Conn::Migrator { .. })
    }
}

#[cfg(test)]
impl Target {
    /// A pool for a test step; the test drops it.
    pub(crate) async fn pool(&self) -> crate::sql::Pool {
        self.open().await.expect("tenant reachable").pool
    }
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

/// `org`'s v2 target. A schema tenant's path is its schema alone: nothing
/// falls through to `public` (decision 1). A database tenant is migrated as
/// `migrator` when given and not its own login.
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
    target_with(pools, org, registry_url, migrator, Setup::Create)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("org `{}`: no schema", org.slug)))
}

/// Whether reaching a schema tenant may create its schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setup {
    Create,
    /// A plan or a read: a missing schema is `None`, never created.
    ReadOnly,
}

/// [`target_as`] with `setup`; `None` for a schema tenant whose schema is
/// missing under [`Setup::ReadOnly`].
async fn target_with<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    registry_url: &str,
    migrator: Option<&MigratorLogin>,
    setup: Setup,
) -> Result<Option<Target>, TenancyError>
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
            refuse_engine_schema(&org.slug, &schema)?;
            let timeout = pg.pool_config().database_pool_acquire_timeout;
            match setup {
                Setup::Create => {
                    super::migrate::ensure_tenant_schema(registry_url, &schema, timeout).await?;
                }
                Setup::ReadOnly => {
                    if !super::migrate::tenant_schema_exists(registry_url, &schema).await? {
                        return Ok(None);
                    }
                }
            }
            // The schema, then schemas holding only extension objects (8.2).
            let path = super::pools::probe_path(
                registry_url,
                &schema,
                super::pools::FallThrough::V2Unit,
                timeout,
            )
            .await?;
            // Named only: [`prepare`] sets it up for a tenant with work.
            let role = pg.tenant_role(&schema).await?.map(|r| r.name);
            return Ok(Some(Target {
                slug: org.slug.clone(),
                scope: LockScope::Schema(schema),
                role,
                conn: Conn::Schema {
                    url: registry_url.to_owned(),
                    path,
                    timeout,
                },
            }));
        }
        #[cfg(not(feature = "postgres"))]
        return Err(TenancyError::Validation(format!(
            "org `{}` is schema-mode, which needs Postgres",
            org.slug
        )));
    }
    let _ = (registry_url, setup);
    if let Some(m) = migrator {
        let url = pools.resolved_database_url(org).await?;
        if let Some(tenant_login) = other_login(&url, m)? {
            return Ok(Some(Target {
                slug: org.slug.clone(),
                scope: LockScope::Session,
                role: Some(tenant_login),
                conn: Conn::Migrator {
                    url,
                    login: m.clone(),
                },
            }));
        }
    }
    Ok(Some(Target {
        slug: org.slug.clone(),
        scope: LockScope::Session,
        role: None,
        conn: Conn::Direct {
            url: pools.tagged_database_url(org).await?,
            timeout: pools.pool_config().database_pool_acquire_timeout,
        },
    }))
}

/// The tenant's own login at `url` when `m` is another one; `None` when
/// they are the same login (the documented gap) or the backend has no logins.
fn other_login(url: &str, m: &MigratorLogin) -> Result<Option<String>, TenancyError> {
    let tenant: Option<String> = match super::BackendKind::of_url(url) {
        #[cfg(feature = "mysql")]
        Some(super::BackendKind::MySql) => Some(
            url.parse::<sqlx::mysql::MySqlConnectOptions>()?
                .get_username()
                .to_owned(),
        ),
        #[cfg(feature = "postgres")]
        Some(super::BackendKind::Postgres) => Some(
            url.parse::<sqlx::postgres::PgConnectOptions>()?
                .get_username()
                .to_owned(),
        ),
        _ => None,
    };
    Ok(tenant.filter(|t| *t != m.user))
}

/// A pool of two on the tenant database at `url` for one step, as `login`
/// when given. A migrator's path on PG is pinned, so names never resolve
/// through a schema the tenant login can create.
#[allow(clippy::unused_async)]
async fn connect_step(
    url: &str,
    login: Option<&MigratorLogin>,
    timeout: Option<std::time::Duration>,
) -> Result<crate::sql::Pool, TenancyError> {
    let _ = (login, timeout);
    match super::BackendKind::of_url(url) {
        #[cfg(feature = "mysql")]
        Some(super::BackendKind::MySql) => {
            let mut opts: sqlx::mysql::MySqlConnectOptions = url.parse()?;
            if let Some(m) = login {
                opts = opts.username(&m.user).password(&m.password);
            }
            let mut po = sqlx::mysql::MySqlPoolOptions::new().max_connections(2);
            if let Some(t) = timeout {
                po = po.acquire_timeout(t);
            }
            Ok(po.connect_with(opts).await?.into())
        }
        #[cfg(feature = "postgres")]
        Some(super::BackendKind::Postgres) => {
            let mut opts: sqlx::postgres::PgConnectOptions = url.parse()?;
            let mut po = sqlx::postgres::PgPoolOptions::new().max_connections(2);
            if let Some(m) = login {
                opts = opts.username(&m.user).password(&m.password);
                po = po.after_connect(|conn, _| {
                    Box::pin(async move {
                        use sqlx::Executor as _;
                        conn.execute(sqlx::raw_sql(MIGRATOR_PATH)).await.map(drop)
                    })
                });
            }
            if let Some(t) = timeout {
                po = po.acquire_timeout(t);
            }
            Ok(po.connect_with(opts).await?.into())
        }
        #[cfg(feature = "sqlite")]
        Some(super::BackendKind::Sqlite) if login.is_none() => {
            let url = super::pools::ensure_sqlite_creates(url);
            let mut po = sqlx::sqlite::SqlitePoolOptions::new().max_connections(2);
            if let Some(t) = timeout {
                po = po.acquire_timeout(t);
            }
            Ok(po.connect(&url).await?.into())
        }
        _ if login.is_some() => Err(TenancyError::Validation(format!(
            "{} is set, but this tenant's backend has no logins",
            MigratorLogin::ENV
        ))),
        _ => Err(TenancyError::Validation(format!(
            "this build cannot open the tenant database at a `{}` URL",
            url.split(':').next().unwrap_or_default()
        ))),
    }
}

/// The migrator's path on a tenant database: no `"$user"` schema the tenant
/// login could create (`pg_catalog` is searched first implicitly).
#[cfg(feature = "postgres")]
const MIGRATOR_PATH: &str = "SET search_path TO public, extensions";

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

/// Schemas the engine owns in the registry database, which no tenant may use.
pub(crate) const ENGINE_SCHEMAS: &[&str] = &[crate::migrate::v2::render::EXTENSIONS_SCHEMA];

/// A stored tenant schema named as one the engine owns is refused (8.2);
/// provisioning refuses the name up front.
///
/// # Errors
/// The schema is reserved.
#[cfg(feature = "postgres")]
pub(crate) fn refuse_engine_schema(slug: &str, schema: &str) -> Result<(), TenancyError> {
    if ENGINE_SCHEMAS.contains(&schema) {
        return Err(TenancyError::Validation(format!(
            "tenant `{slug}` uses schema `{schema}`, which the migration engine owns: \
             move it to another schema (`migrate-storage`) before migrating"
        )));
    }
    Ok(())
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
    /// Set by a shutdown signal: each tenant stops after its unit in flight,
    /// no further tenant starts, and the record says `stopped` (6.11).
    pub(crate) stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Boot: give up waiting on the fleet lock (or a tenant's) once the
    /// holder's heartbeat is this old; `None` waits on.
    pub(crate) stale_after: Option<std::time::Duration>,
    /// How often the fleet record's heartbeat is refreshed; `None` = 30 s.
    pub(crate) heartbeat_every: Option<std::time::Duration>,
}

impl FleetOptions {
    fn stopping(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst))
    }
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
    /// A shutdown signal stopped the run; it resumes on the next one.
    pub(crate) shutdown: bool,
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

    /// A shutdown stopped the run: the process exits non-zero (6.11).
    pub(crate) fn stop_error(&self) -> Option<TenancyError> {
        self.shutdown.then(|| {
            TenancyError::Validation(format!(
                "{}: a shutdown signal stopped the fleet run",
                run::STOPPED
            ))
        })
    }

    /// One outcome per tenant: a later stage's adds to the earlier one.
    fn record(&mut self, o: TenantOutcome) {
        match self.tenants.iter_mut().find(|t| t.slug == o.slug) {
            Some(t) => {
                t.applied.extend(o.applied);
                if o.error_class.is_some() {
                    t.error_class = o.error_class;
                    t.error = o.error;
                }
            }
            None => self.tenants.push(o),
        }
    }
}

fn migrate_err(e: run::RunError) -> TenancyError {
    TenancyError::Migrate(e.into())
}

/// The registry chain, then every tenant's, under the fleet lock and a
/// fleet record (8.3). A registry failure is an error; a tenant failure is
/// recorded in the registry cache (its error class only) and the run goes
/// on, within `max_failures`. Every refusal comes before any write: the
/// record refuses another plan's unfinished run, then every tenant is
/// planned (against the registry as its chain will leave it) and one
/// blocker refuses them all. Canaries go first, in the order given; every
/// tenant reaches its expand head before any contract file runs; tenants
/// added while it runs are picked up before it ends. A waiter gives up
/// only on a holder whose heartbeat is older than `stale_after` (6.11).
///
/// # Errors
/// The registry chain, a blocker in any tenant, another unfinished fleet
/// plan, an unknown canary, a hung lock holder, or reading the orgs.
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
    let got_lock = std::sync::atomic::AtomicBool::new(false);
    let locked = run::fleet_locked(&registry, || async {
        got_lock.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(fleet_body(pools, &registry, dir, registry_url, fo).await)
    });
    let result: Result<Result<FleetReport, TenancyError>, crate::migrate::MigrateError> =
        match fo.stale_after {
            None => locked.await,
            Some(stale) => tokio::select! {
                r = locked => r,
                e = stale_fleet_holder(&registry, stale, &got_lock) => return Err(e),
            },
        };
    result?
}

/// Resolves with an error once the fleet lock's holder has a fleet record
/// whose heartbeat is older than `stale`, while this run still waits; pends
/// after that. The same stale record must stay so for `stale / 2`: a live
/// holder that just took the lock closes a dead run's record first.
async fn stale_fleet_holder(
    registry: &crate::sql::Pool,
    stale: std::time::Duration,
    got_lock: &std::sync::atomic::AtomicBool,
) -> TenancyError {
    use std::sync::atomic::Ordering::SeqCst;
    let every = (stale / 4).clamp(
        std::time::Duration::from_millis(100),
        std::time::Duration::from_secs(5),
    );
    let mut seen: Option<(String, std::time::Instant)> = None;
    let mut logged = std::time::Instant::now();
    loop {
        tokio::time::sleep(every).await;
        if got_lock.load(SeqCst) {
            return std::future::pending().await;
        }
        // A missing record table or a read error: keep waiting.
        let found = crate::migrate::v2::ledger::fleet_stale(registry, stale)
            .await
            .ok()
            .flatten();
        if got_lock.load(SeqCst) {
            return std::future::pending().await;
        }
        match (found, &seen) {
            (Some((run, age)), Some((was, since)))
                if *was == run && since.elapsed() >= (stale / 2).max(every) =>
            {
                return TenancyError::Validation(format!(
                    "the fleet lock is held by fleet run {run} whose heartbeat is {age}s old: it \
                     looks hung; stopping instead of waiting on it"
                ));
            }
            (Some((run, _)), Some((was, _))) if *was == run => {}
            (Some((run, _)), _) => seen = Some((run, std::time::Instant::now())),
            (None, _) => seen = None,
        }
        if logged.elapsed() >= std::time::Duration::from_secs(30) {
            logged = std::time::Instant::now();
            tracing::info!(target: "rustango::tenancy", "still waiting for the fleet lock");
        }
    }
}

/// `fut` while the fleet record `run_id`'s heartbeat is refreshed every
/// `every` (30 s by default), so waiters see a live holder (6.11).
pub(crate) async fn beating<F: std::future::Future>(
    registry: &crate::sql::Pool,
    run_id: &str,
    every: Option<std::time::Duration>,
    fut: F,
) -> F::Output {
    tokio::select! {
        out = fut => out,
        never = fleet_heartbeats(registry, run_id, every.unwrap_or(run::HEARTBEAT_EVERY)) => {
            match never {}
        }
    }
}

async fn fleet_heartbeats(
    registry: &crate::sql::Pool,
    run_id: &str,
    every: std::time::Duration,
) -> std::convert::Infallible {
    loop {
        tokio::time::sleep(every).await;
        // Best effort: waiters only read it.
        if let Err(e) = crate::migrate::v2::ledger::fleet_beat(registry, run_id).await {
            tracing::warn!(target: "rustango::tenancy", error = %e, "fleet heartbeat failed");
        }
    }
}

/// The fleet's tenants by slug order: every org (or the active ones), and
/// what it cannot count on: unreadable rows and, with `--active-only`, the
/// inactive tenants left out.
async fn fleet_orgs(
    registry: &crate::sql::Pool,
    fo: &FleetOptions,
) -> Result<(Vec<Org>, Vec<String>), TenancyError> {
    use crate::core::Model as _;
    // A fresh registry has no orgs table until its chain runs.
    if !crate::migrate::try_table_exists_here(registry, Org::SCHEMA.table).await? {
        return Ok((Vec::new(), Vec::new()));
    }
    let (orgs, unreadable) =
        Org::fetch_reporting(Org::objects().order_by(&[("slug", false)]), registry)
            .await
            .map_err(crate::migrate::MigrateError::from)?;
    let mut unknown: Vec<String> = unreadable
        .iter()
        .map(|r| format!("{} (unreadable)", r.id))
        .collect();
    if !unknown.is_empty() {
        tracing::warn!(
            target: "rustango::tenancy",
            unreadable = unknown.len(),
            "org rows the fleet could not read are not migrated"
        );
    }
    let (orgs, inactive): (Vec<Org>, Vec<Org>) =
        orgs.into_iter().partition(|o| !fo.active_only || o.active);
    unknown.extend(inactive.iter().map(|o| format!("{} (inactive)", o.slug)));
    Ok((orgs, unknown))
}

/// A `--canary` slug no tenant of this run has is a typo: refused.
fn refuse_unknown_canaries(fo: &FleetOptions, orgs: &[Org]) -> Result<(), TenancyError> {
    let unknown: Vec<&str> = fo
        .canaries
        .iter()
        .filter(|c| !orgs.iter().any(|o| o.slug == **c))
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "--canary names no tenant of this run: {}; nothing was changed",
        unknown.join(", ")
    )))
}

/// A tenant run's options.
pub(crate) fn tenant_opts(t: &Target) -> Options {
    Options {
        run_as: RunAs::Tenant {
            own_database: t.own_database(),
        },
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
    let (orgs, unknown) = fleet_orgs(registry, fo).await?;
    refuse_unknown_canaries(fo, &orgs)?;
    // Read once for the whole run (every tenant plans from it).
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    // The record first: another plan's unfinished run refuses before any write.
    report.run_id = run::new_run_id();
    crate::migrate::v2::ledger::fleet_open(
        registry,
        &report.run_id,
        "up",
        &run::plan_hash(&project),
        fo.abandon,
        None,
    )
    .await?;
    let run_id = report.run_id.clone();
    // Boxed: the fleet future is large in debug builds.
    let work = Box::pin(fleet_work(
        pools,
        registry,
        &project,
        registry_url,
        fo,
        orgs,
        unknown,
        &mut report,
    ));
    let result = beating(registry, &run_id, fo.heartbeat_every, work).await;
    let state = if report.shutdown {
        "stopped"
    } else if result.is_err() || report.failed().next().is_some() || report.stopped {
        "failed"
    } else {
        "succeeded"
    };
    crate::migrate::v2::ledger::fleet_close(registry, &run_id, state).await?;
    result.map(|()| report)
}

/// The run behind the record: read and plan every tenant, refuse on any
/// blocker, then the extension move, the registry chain and the tenants.
#[allow(clippy::too_many_arguments)]
async fn fleet_work<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    project: &crate::migrate::v2::load::Project,
    registry_url: &str,
    fo: &FleetOptions,
    orgs: Vec<Org>,
    mut unknown: Vec<String>,
    report: &mut FleetReport,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    #[cfg(feature = "postgres")]
    for o in orgs
        .iter()
        .filter(|o| o.storage_mode == StorageMode::Schema)
    {
        refuse_engine_schema(&o.slug, o.effective_schema())?;
    }
    // Every target's state from the files, before any tenant is read.
    crate::migrate::v2::load::replay(project).map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let k = fo.parallel.max(1);
    let (surveyed, unreachable) =
        survey_all(pools, &orgs, registry_url, fo.migrator.as_ref(), project, k).await;
    unknown.extend(
        unreachable
            .iter()
            .map(|t| format!("{} (unreachable)", t.slug)),
    );
    report.tenants.extend(unreachable);
    // `requires: tenants:<id>` holds when every tenant has applied it.
    let mut everywhere: Option<std::collections::BTreeSet<String>> = None;
    for s in &surveyed {
        everywhere = Some(match everywhere {
            None => s.applied.clone(),
            Some(all) => all.intersection(&s.applied).cloned().collect(),
        });
    }
    let reg = Options {
        run_as: RunAs::Registry,
        tenants_applied: Some(everywhere.unwrap_or_default()),
        tenants_unknown: unknown,
        stop: fo.stop.clone(),
        stale_after: fo.stale_after,
        ..Options::default()
    };
    // One tip for every tenant: each distinct applied set plans once (4.4).
    let plan = std::sync::Arc::new(
        crate::migrate::v2::plan::TipPlanner::new(
            project,
            crate::migrate::v2::state::Target::Tenant,
        )
        .map_err(|e| migrate_err(run::RunError::Load(e)))?,
    );
    // Planned as the registry chain will leave it, before it writes.
    let expected = run::registry_after(registry, project, &reg)
        .await
        .map_err(migrate_err)?;
    let planning = Fleet {
        registry,
        project,
        registry_applied: &expected,
        run_id: &report.run_id,
        plan: plan.clone(),
        parallel: k,
        fo,
    };
    planning.refuse_blockers(&surveyed).await?;
    // Writes from here.
    #[cfg(feature = "postgres")]
    if registry.dialect().kind() == crate::sql::DialectKind::Postgres {
        extensions_home(registry).await?;
    }
    let ran = run::migrate_loaded(registry, project, &reg)
        .await
        .map_err(migrate_err)?;
    report.registry = ran.applied;
    if ran.stopped.is_some() {
        report.shutdown = true;
        return Ok(());
    }
    let registry_applied = run::applied_ids(registry, &reg)
        .await
        .map_err(migrate_err)?;
    #[cfg(feature = "postgres")]
    if registry.dialect().kind() == crate::sql::DialectKind::Postgres
        && orgs.iter().any(|o| o.storage_mode == StorageMode::Schema)
    {
        refuse_missing_extensions(registry, &run::registry_extensions(project)).await?;
    }
    crate::migrate::v2::ledger::cache_ensure(registry).await?;
    let run_id = report.run_id.clone();
    let fleet = Fleet {
        registry,
        project,
        registry_applied: &registry_applied,
        run_id: &run_id,
        plan,
        parallel: k,
        fo,
    };
    let seen = orgs.iter().map(|o| o.slug.clone()).collect();
    fleet_rounds(pools, &fleet, registry_url, surveyed, seen, report).await
}

/// The fleet's rounds: the tenants known, then those provisioned meanwhile.
/// The first round was planned before the registry chain; later rounds are
/// planned here.
#[allow(clippy::too_many_lines)]
async fn fleet_rounds<DB: Database>(
    pools: &TenantPools<DB>,
    fleet: &Fleet<'_>,
    registry_url: &str,
    mut surveyed: Vec<Surveyed>,
    mut seen: std::collections::BTreeSet<String>,
    report: &mut FleetReport,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let (registry, fo) = (fleet.registry, fleet.fo);
    // Shared by the buffered tenant futures; atomic so the fleet future is `Send`.
    let failures = AtomicUsize::new(report.tenants.len());
    let bump = |n: usize| failures.fetch_add(n, SeqCst);
    let mut planned = true;
    loop {
        if !std::mem::replace(&mut planned, false) {
            fleet.refuse_blockers(&surveyed).await?;
        }
        let (current, work): (Vec<Surveyed>, Vec<Surveyed>) = std::mem::take(&mut surveyed)
            .into_iter()
            .partition(|s| s.current);
        fleet.record_current(&current, report).await?;
        // Contract files only once every tenant reached its expand head.
        let gated = !fo.allow_mixed_heads
            && work.iter().any(|s| {
                run::contract_pending(
                    fleet.project,
                    crate::migrate::v2::state::Target::Tenant,
                    &s.applied,
                )
            });
        // Schemas and login roles are set up only now, for tenants with work.
        let mut ready = Vec::new();
        for s in work {
            match setup(pools, s, registry_url, fo.migrator.as_ref()).await {
                Ok(t) => ready.push(t),
                Err(o) => {
                    bump(1);
                    report.record(o);
                }
            }
        }
        let stages: &[bool] = if fo.expand_only || gated {
            if fo.expand_only {
                &[true]
            } else {
                &[true, false]
            }
        } else {
            &[false]
        };
        // Canaries first, one by one, in the order given; then the rest.
        let canaries: Vec<&Target> = fo
            .canaries
            .iter()
            .filter_map(|c| ready.iter().find(|t| t.slug == *c))
            .collect();
        let rest: Vec<&Target> = ready
            .iter()
            .filter(|t| !fo.canaries.contains(&t.slug))
            .collect();
        for &expand in stages {
            let before = failures.load(SeqCst);
            for (n, t) in canaries.iter().enumerate() {
                if fo.stopping() {
                    report.shutdown = true;
                    report
                        .skipped
                        .extend(canaries[n..].iter().chain(&rest).map(|t| t.slug.clone()));
                    break;
                }
                let out = fleet.migrate_one(t, expand).await?;
                let failed = out.error_class.is_some();
                if failed {
                    bump(1);
                }
                report.record(out);
                if failed && !fo.stopping() {
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
            if report.stopped || report.shutdown {
                break;
            }
            use futures_util::StreamExt as _;
            let mut stream = futures_util::stream::iter(rest.iter().map(|t| async {
                if fo.stopping() {
                    return Ok(Err((t.slug.clone(), true)));
                }
                if fo.max_failures.is_some_and(|m| failures.load(SeqCst) > m) {
                    return Ok(Err((t.slug.clone(), false)));
                }
                fleet.migrate_one(t, expand).await.map(Ok)
            }))
            .buffered(fleet.parallel);
            while let Some(out) = stream.next().await {
                match out? {
                    Ok(o) => {
                        if o.error_class.is_some() {
                            bump(1);
                        }
                        report.record(o);
                    }
                    Err((slug, shutdown)) => {
                        if shutdown {
                            report.shutdown = true;
                        } else {
                            report.stopped = true;
                        }
                        report.skipped.push(slug);
                    }
                }
            }
            drop(stream);
            if fo.stopping() {
                report.shutdown = true;
            }
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
            if report.stopped || report.shutdown {
                break;
            }
        }
        if report.stopped || report.shutdown {
            break;
        }
        // Tenants provisioned while the fleet ran: migrate them too, until none is new.
        let fresh: Vec<Org> = fleet_orgs(registry, fo)
            .await?
            .0
            .into_iter()
            .filter(|o| seen.insert(o.slug.clone()))
            .collect();
        if fresh.is_empty() {
            break;
        }
        let (s, unreachable) = survey_all(
            pools,
            &fresh,
            registry_url,
            fo.migrator.as_ref(),
            fleet.project,
            fleet.parallel,
        )
        .await;
        bump(unreachable.len());
        report.tenants.extend(unreachable);
        surveyed = s;
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
/// replaces (9.6): each database whose ledger records that chain (the
/// registry, each tenant, inactive ones too, or the configured database)
/// that ran part of them and not the checkpoint. A tenant that cannot be
/// reached or read counts.
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
    use crate::migrate::v2::state::Target as Chain;
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let c = squash_checkpoint(&project, id)
        .ok_or_else(|| TenancyError::Validation(format!("no squash checkpoint `{id}`")))?
        .file
        .clone();
    let ledger = crate::migrate::v2::ledger::Ledger::default_names();
    let behind_on = |pool: crate::sql::Pool| {
        let (ledger, c) = (&ledger, &c);
        async move {
            let applied: std::collections::HashSet<String> = ledger
                .applied(&pool)
                .await?
                .into_iter()
                .filter(|r| {
                    r.app == c.app && r.state == crate::migrate::v2::ledger::RowState::Applied
                })
                .map(|r| r.id)
                .collect();
            Ok::<bool, TenancyError>(needs_replaced(c, &applied))
        }
    };
    let registry = pools.registry_pool();
    let mut behind = Vec::new();
    match c.target {
        Chain::Database => {
            let name = c.database.clone().unwrap_or_default();
            match crate::databases::get(&name) {
                Some(pool) => {
                    if behind_on(pool).await? {
                        behind.push(format!("database `{name}`"));
                    }
                }
                None => behind.push(format!("database `{name}` (not configured)")),
            }
            return Ok(PruneGate::Fleet { behind });
        }
        Chain::Registry | Chain::EveryDatabase => {
            if behind_on(registry.clone()).await? {
                behind.push("the registry".to_owned());
            }
            if c.target == Chain::Registry {
                return Ok(PruneGate::Fleet { behind });
            }
        }
        Chain::Tenant => {}
    }
    let (orgs, unknown) = fleet_orgs(&registry, &FleetOptions::default()).await?;
    behind.extend(unknown);
    // Schema tenants share the registry database, so never run `every_database`.
    let orgs: Vec<Org> = orgs
        .into_iter()
        .filter(|o| c.target == Chain::Tenant || o.storage_mode != StorageMode::Schema)
        .collect();
    for org in &orgs {
        let t = match target_as(pools, org, registry_url, None).await {
            Ok(t) => t,
            Err(_) => {
                behind.push(format!("{} (unreachable)", org.slug));
                continue;
            }
        };
        let open = match t.open().await {
            Ok(o) => o,
            Err(_) => {
                behind.push(format!("{} (unreachable)", org.slug));
                continue;
            }
        };
        let r = behind_on(open.pool.clone()).await;
        open.close().await;
        if r? {
            behind.push(t.slug.clone());
        }
    }
    Ok(PruneGate::Fleet { behind })
}

/// One tenant after the fleet read its ledgers.
struct Surveyed {
    org: Org,
    /// `None`: a schema tenant whose schema is not made yet.
    target: Option<Target>,
    applied: std::collections::BTreeSet<String>,
    /// Every file applied as on disk: nothing to plan or run.
    current: bool,
}

/// A tenant the fleet could not reach or set up: its error class, and the
/// error itself in the operator's log.
fn unreachable_of(slug: &str, e: &TenancyError) -> TenantOutcome {
    tracing::warn!(
        target: "rustango::tenancy",
        slug,
        error = %e,
        "tenant unreachable; it is not migrated"
    );
    TenantOutcome {
        slug: slug.to_owned(),
        applied: Vec::new(),
        error_class: Some("unreachable".into()),
        error: Some(e.to_string()),
    }
}

/// Each org's target and ledgers, read only, `k` at a time, each
/// connection closed after; an outcome for each one unreachable. Nothing
/// is set up here: [`setup`] does that for tenants with work.
async fn survey_all<DB: Database>(
    pools: &TenantPools<DB>,
    orgs: &[Org],
    registry_url: &str,
    migrator: Option<&MigratorLogin>,
    project: &crate::migrate::v2::load::Project,
    k: usize,
) -> (Vec<Surveyed>, Vec<TenantOutcome>)
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use futures_util::StreamExt as _;
    let one = |org: &Org| {
        let org = org.clone();
        async move {
            let target =
                match target_with(pools, &org, registry_url, migrator, Setup::ReadOnly).await {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        return Ok(Surveyed {
                            org,
                            target: None,
                            applied: std::collections::BTreeSet::new(),
                            current: false,
                        })
                    }
                    Err(e) => return Err(unreachable_of(&org.slug, &e)),
                };
            survey_target(org, target, project)
                .await
                .map_err(|(slug, e)| unreachable_of(&slug, &e))
        }
    };
    let results: Vec<Result<Surveyed, TenantOutcome>> = futures_util::stream::iter(orgs.iter())
        .map(one)
        .buffered(k)
        .collect()
        .await;
    let (mut surveyed, mut unreachable) = (Vec::new(), Vec::new());
    for r in results {
        match r {
            Ok(s) => surveyed.push(s),
            Err(o) => unreachable.push(o),
        }
    }
    (surveyed, unreachable)
}

/// A tenant with work, ready to run: a schema tenant's schema is made and
/// its path read again (the extension move may have changed it), and its
/// login role set up (or its password reset to the current secret).
async fn setup<DB: Database>(
    pools: &TenantPools<DB>,
    s: Surveyed,
    registry_url: &str,
    migrator: Option<&MigratorLogin>,
) -> Result<Target, TenantOutcome>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let target = match s.target {
        Some(t) if t.own_database() => t,
        _ => target_as(pools, &s.org, registry_url, migrator)
            .await
            .map_err(|e| unreachable_of(&s.org.slug, &e))?,
    };
    prepare(pools, &target)
        .await
        .map_err(|e| unreachable_of(&target.slug, &e))?;
    Ok(target)
}

/// A schema tenant with work gets its login role set up (or its password
/// reset to the current secret) before its run.
async fn prepare<DB: Database>(pools: &TenantPools<DB>, t: &Target) -> Result<(), TenancyError> {
    #[cfg(feature = "postgres")]
    if let (LockScope::Schema(schema), Some(_)) = (&t.scope, &t.role) {
        if let Some(pg) =
            (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
        {
            pg.ensure_tenant_role(schema).await?;
        }
    }
    let _ = (pools, t);
    Ok(())
}

/// PG database mode with a separate migrator: the migrator owns the
/// tenant's objects and the tenant login keeps rows only (8.1).
async fn secure_migrator(pool: &crate::sql::Pool, login: Option<&str>) -> Result<(), TenancyError> {
    #[cfg(feature = "postgres")]
    if let (crate::sql::Pool::Postgres(pg), Some(login)) = (pool, login) {
        return super::schema_role::secure_database_login(pg, login).await;
    }
    let _ = (pool, login);
    Ok(())
}

/// `target`'s ledgers read on a connection closed after. A migrator
/// secures the tenant database before it reads anything there.
async fn survey_target(
    org: Org,
    target: Target,
    project: &crate::migrate::v2::load::Project,
) -> Result<Surveyed, (String, TenancyError)> {
    let open = match target.open().await {
        Ok(o) => o,
        Err(e) => return Err((target.slug, e)),
    };
    let s = async {
        if target.as_migrator() {
            secure_migrator(&open.pool, target.role.as_deref()).await?;
        }
        run::survey(&open.pool, project, &tenant_opts(&target))
            .await
            .map_err(migrate_err)
    }
    .await;
    open.close().await;
    match s {
        Ok(s) => Ok(Surveyed {
            org,
            current: s.current,
            applied: s.applied,
            target: Some(target),
        }),
        Err(e) => Err((target.slug, e)),
    }
}

/// The newest tenant migration id applied: the registry cache's head.
fn head_of(ids: &std::collections::BTreeSet<String>) -> Option<String> {
    ids.iter().filter(|i| !i.contains(':')).max().cloned()
}

/// What every tenant run of one fleet run shares.
struct Fleet<'a> {
    registry: &'a crate::sql::Pool,
    project: &'a crate::migrate::v2::load::Project,
    registry_applied: &'a std::collections::BTreeSet<String>,
    run_id: &'a str,
    plan: std::sync::Arc<crate::migrate::v2::plan::TipPlanner>,
    /// Tenants worked on at once, and so the tenant connections open.
    parallel: usize,
    fo: &'a FleetOptions,
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
            stop: self.fo.stop.clone(),
            stale_after: self.fo.stale_after,
            ..tenant_opts(t)
        }
    }

    /// Tenants with nothing to run: the registry cache says applied
    /// (written only when it did not). Their ledgers were locked when they
    /// last ran, and role set-up locks them again.
    async fn record_current(
        &self,
        current: &[Surveyed],
        report: &mut FleetReport,
    ) -> Result<(), TenancyError> {
        if current.is_empty() {
            return Ok(());
        }
        let cache: std::collections::HashMap<String, (String, Option<String>)> =
            crate::migrate::v2::ledger::cache_all(self.registry)
                .await?
                .into_iter()
                .map(|(slug, state, head, _)| (slug, (state, head)))
                .collect();
        for s in current {
            let head = head_of(&s.applied);
            let want = ("applied".to_owned(), head.clone());
            if cache.get(&s.org.slug) != Some(&want) {
                crate::migrate::v2::ledger::cache_put(
                    self.registry,
                    &s.org.slug,
                    "applied",
                    head.as_deref(),
                    None,
                    self.run_id,
                )
                .await?;
            }
            report.record(TenantOutcome {
                slug: s.org.slug.clone(),
                applied: Vec::new(),
                error_class: None,
                error: None,
            });
        }
        Ok(())
    }

    /// Plan every tenant with work before any change, `parallel` at a
    /// time: a refusal anywhere stops all. A schema not made yet has
    /// nothing to refuse.
    async fn refuse_blockers(&self, surveyed: &[Surveyed]) -> Result<(), TenancyError> {
        use futures_util::StreamExt as _;
        let mut blockers = Vec::new();
        // A schema not made yet has nothing applied: its files alone tell.
        let fresh: Vec<&str> = surveyed
            .iter()
            .filter(|s| !s.current && s.target.is_none())
            .map(|s| s.org.slug.as_str())
            .collect();
        if !fresh.is_empty() {
            let opts = Options {
                registry_applied: Some(self.registry_applied.clone()),
                run_as: RunAs::Tenant {
                    own_database: false,
                },
                ..Options::default()
            };
            let b = crate::migrate::v2::state::Backend::Postgres;
            if let Err(e) = run::fresh_refusal(self.project, &opts, b) {
                blockers.push(format!("  - {}: {e}", fresh.join(", ")));
            }
        }
        let work: Vec<&Target> = surveyed
            .iter()
            .filter(|s| !s.current)
            .filter_map(|s| s.target.as_ref())
            .collect();
        let found: Vec<Result<Option<String>, TenancyError>> =
            futures_util::stream::iter(work.iter().map(|t| self.check_one(t)))
                .buffered(self.parallel)
                .collect()
                .await;
        for (t, r) in work.iter().zip(found) {
            if let Some(b) = r? {
                blockers.push(format!("  - {}: {b}", t.slug));
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

    /// What refuses `t`'s run, on a connection closed after.
    async fn check_one(&self, t: &Target) -> Result<Option<String>, TenancyError> {
        let open = t.open().await?;
        let r = self.blocker(t, &open.pool).await;
        open.close().await;
        r
    }

    /// What refuses `t`'s run, from a dry run on its open pool.
    async fn blocker(
        &self,
        t: &Target,
        pool: &crate::sql::Pool,
    ) -> Result<Option<String>, TenancyError> {
        if let Some(login) = &t.role {
            if let Some(wide) = crate::migrate::v2::ledger::wide_grants(pool, login).await? {
                return Ok(Some(wide));
            }
        }
        match run::migrate_loaded(pool, self.project, &self.opts(t, true, false)).await {
            Err(e) if e.code().is_some() => Ok(Some(e.to_string())),
            _ => Ok(None),
        }
    }

    /// One tenant's run, recorded in the registry cache. A failed or
    /// stopped tenant keeps the head it reached.
    async fn migrate_one(&self, t: &Target, expand: bool) -> Result<TenantOutcome, TenancyError> {
        use crate::migrate::v2::ledger::cache_put;
        cache_put(self.registry, &t.slug, "running", None, None, self.run_id).await?;
        let (ran, head) = match t.open().await {
            Ok(open) => {
                let r = self.run_open(t, &open.pool, expand).await;
                open.close().await;
                r
            }
            Err(e) => (Err(("unreachable", e.to_string())), None),
        };
        Ok(match ran {
            Ok(applied) => {
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
                    applied,
                    error_class: None,
                    error: None,
                }
            }
            Err((class, e)) => {
                cache_put(
                    self.registry,
                    &t.slug,
                    "failed",
                    head.as_deref(),
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
        })
    }

    /// `t`'s run on its open pool: what it applied (a stop is a failure of
    /// class `stopped`), and its head after either way.
    async fn run_open(
        &self,
        t: &Target,
        pool: &crate::sql::Pool,
        expand: bool,
    ) -> (Result<Vec<String>, (&'static str, String)>, Option<String>) {
        let ran = match run::migrate_loaded(pool, self.project, &self.opts(t, false, expand)).await
        {
            Ok(rep) => match rep.stopped {
                Some(why) => Err(("stopped", format!("{}: {why}", run::STOPPED))),
                None => Ok(rep.applied),
            },
            Err(e) => Err((run::error_class(&e), e.to_string())),
        };
        let head = run::applied_ids(pool, &tenant_opts(t))
            .await
            .ok()
            .and_then(|ids| head_of(&ids));
        (ran, head)
    }
}

/// Which tenants a per-database `dbmigrate` verb works on.
#[derive(Clone, Copy)]
pub(crate) enum Tenants<'a> {
    All,
    One(&'a str),
}

/// A tenant as a per-database verb finds it, writing nothing.
pub(crate) enum Reached {
    Target(Target),
    /// A schema tenant whose schema does not exist yet.
    MissingSchema,
    Unreachable(TenancyError),
}

/// The tenants' targets in slug order, inactive ones too, each reached as
/// the fleet reaches it but read-only: a missing schema is not created.
///
/// # Errors
/// Reading the orgs, or `One` naming no tenant.
pub(crate) async fn targets<DB: Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    which: Tenants<'_>,
) -> Result<Vec<(String, Reached)>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let (mut orgs, _) = fleet_orgs(&pools.registry_pool(), &FleetOptions::default()).await?;
    if let Tenants::One(slug) = which {
        orgs.retain(|o| o.slug == slug);
        if orgs.is_empty() {
            return Err(TenancyError::Validation(format!("no tenant `{slug}`")));
        }
    }
    let migrator = MigratorLogin::from_env();
    let mut out = Vec::new();
    for org in &orgs {
        let t =
            match target_with(pools, org, registry_url, migrator.as_ref(), Setup::ReadOnly).await {
                Ok(Some(t)) => Reached::Target(t),
                Ok(None) => Reached::MissingSchema,
                Err(e) => Reached::Unreachable(e),
            };
        out.push((org.slug.clone(), t));
    }
    Ok(out)
}

/// The backends of the registry and of every tenant, once each.
///
/// # Errors
/// Reading the orgs.
pub(crate) async fn backends(
    registry: &crate::sql::Pool,
) -> Result<Vec<crate::migrate::v2::state::Backend>, TenancyError> {
    use crate::migrate::v2::state::Backend;
    let mut out = vec![run::backend_of(registry)];
    for org in fleet_orgs(registry, &FleetOptions::default()).await?.0 {
        let b = match org.backend_kind {
            super::BackendKind::Postgres => Backend::Postgres,
            super::BackendKind::MySql => Backend::Mysql,
            super::BackendKind::Sqlite => Backend::Sqlite,
        };
        if !out.contains(&b) {
            out.push(b);
        }
    }
    Ok(out)
}

/// After a tenant's rollback: the registry cache holds its new head.
///
/// # Errors
/// Writing the cache.
pub(crate) async fn record_head(
    registry: &crate::sql::Pool,
    t: &Target,
    pool: &crate::sql::Pool,
) -> Result<(), TenancyError> {
    let head = run::applied_ids(pool, &tenant_opts(t))
        .await
        .map_err(migrate_err)?;
    crate::migrate::v2::ledger::cache_ensure(registry).await?;
    crate::migrate::v2::ledger::cache_put(
        registry,
        &t.slug,
        "applied",
        head_of(&head).as_deref(),
        None,
        &run::new_run_id(),
    )
    .await?;
    Ok(())
}

/// What a fleet run would run, with nothing written: the registry's plan,
/// then each tenant's against the registry's applied set (or why it is
/// refused).
///
/// # Errors
/// The registry's dry run, a load error, or reading the orgs.
pub(crate) async fn plan_fleet<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
) -> Result<Vec<(String, Result<Vec<String>, String>)>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let reg = Options {
        run_as: RunAs::Registry,
        dry_run: true,
        ..Options::default()
    };
    let mut out = vec![(
        "registry".to_owned(),
        Ok(run::migrate_loaded(&registry, &project, &reg)
            .await
            .map_err(migrate_err)?
            .planned),
    )];
    let applied = run::applied_ids(&registry, &reg)
        .await
        .map_err(migrate_err)?;
    for (slug, t) in targets(pools, registry_url, Tenants::All).await? {
        let planned = match t {
            Reached::Unreachable(e) => Err(e.to_string()),
            Reached::MissingSchema => Ok(vec!["schema missing, would create".to_owned()]),
            Reached::Target(t) => match t.open().await {
                Err(e) => Err(e.to_string()),
                Ok(open) => {
                    let opts = Options {
                        dry_run: true,
                        registry_applied: Some(applied.clone()),
                        ..tenant_opts(&t)
                    };
                    let r = run::migrate_loaded(&open.pool, &project, &opts).await;
                    open.close().await;
                    r.map(|r| r.planned).map_err(|e| e.to_string())
                }
            },
        };
        out.push((format!("tenant {slug}"), planned));
    }
    Ok(out)
}

/// Engine v2 for one tenant outside a fleet run (provisioning): the
/// fleet's own per-tenant run, against the registry's applied set.
///
/// # Errors
/// An unreachable tenant, a blocker, or a failed migration.
pub(crate) async fn migrate_tenant<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    dir: &Path,
    registry_url: &str,
) -> Result<TenantOutcome, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let project = crate::migrate::v2::load::load_project(dir)
        .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    let reg = Options {
        run_as: RunAs::Registry,
        ..Options::default()
    };
    let registry_applied = run::applied_ids(&registry, &reg)
        .await
        .map_err(migrate_err)?;
    let plan = crate::migrate::v2::plan::TipPlanner::new(
        &project,
        crate::migrate::v2::state::Target::Tenant,
    )
    .map_err(|e| migrate_err(run::RunError::Load(e)))?;
    crate::migrate::v2::ledger::cache_ensure(&registry).await?;
    let run_id = run::new_run_id();
    let fo = FleetOptions::default();
    let fleet = Fleet {
        registry: &registry,
        project: &project,
        registry_applied: &registry_applied,
        run_id: &run_id,
        plan: std::sync::Arc::new(plan),
        parallel: 1,
        fo: &fo,
    };
    let migrator = MigratorLogin::from_env();
    let target = target_as(pools, org, registry_url, migrator.as_ref()).await?;
    prepare(pools, &target).await?;
    let one = [survey_target(org.clone(), target, &project)
        .await
        .map_err(|(_, e)| e)?];
    let mut report = FleetReport::default();
    if one[0].current {
        fleet.record_current(&one, &mut report).await?;
        return Ok(report.tenants.remove(0));
    }
    let target = one[0].target.as_ref().expect("surveyed with a target");
    if let Some(b) = fleet.check_one(target).await? {
        return Err(TenancyError::Validation(format!(
            "tenant `{}` is blocked; nothing was changed: {b}",
            org.slug
        )));
    }
    let out = fleet.migrate_one(target, false).await?;
    match &out.error {
        Some(e) => Err(TenancyError::Validation(format!(
            "tenant `{}` did not migrate: {e}",
            org.slug
        ))),
        None => Ok(out),
    }
}

#[cfg(test)]
#[path = "migrate_v2_tests.rs"]
mod tests;
