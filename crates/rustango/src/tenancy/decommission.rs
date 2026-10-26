//! Taking a tenant out of service, soft or hard.
//!
//! These steps lived inside `drop-tenant` and `purge-tenant`, welded to
//! a CLI by a `pub(super)` visibility, an `args: &[String]` they parsed
//! themselves, and a `W: Write` they reported through — the same shape
//! `provision.rs` was in before it was extracted. Anything that is not
//! a terminal had to fake an `argv`.
//!
//! ## Soft and hard are different operations, not a flag
//!
//! [`Action::Deactivate`] flips `active` and preserves everything. The
//! resolver already filters on that column, so the tenant stops serving
//! immediately and can be brought back.
//!
//! [`Action::Purge`] destroys the storage and deletes the row. It is
//! unrecoverable, and `purge_database` has to be passed explicitly for
//! a database-mode tenant: dropping a whole database is a bigger act
//! than dropping a schema, and the caller should have to say so.
//!
//! [`Action::Deactivate`]: crate::tenancy::decommission::Action::Deactivate
//! [`Action::Purge`]: crate::tenancy::decommission::Action::Purge

use sqlx::Database;

use super::db_identity::DbIdentity;
use super::error::TenancyError;
use super::operation::{OpReporter, OperationObserver, OperationStep as Step};
use super::org::{Org, StorageMode};
use super::org_host::OrgHost;
use super::pools::TenantPools;
use super::provision_store::{RunKind, RunText};
use super::server_admin::ServerAdmin;
use super::server_tenant::ServerTenant;
use crate::core::Column as _;
use crate::sql::{FetcherPool as _, UpdaterPool as _};

/// What to do to the tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `active = false`. Reversible; nothing is destroyed.
    Deactivate,
    /// Drop the storage and the registry row. Unrecoverable.
    Purge {
        /// Required for a database-mode tenant, where purging means
        /// `DROP DATABASE` rather than `DROP SCHEMA`.
        purge_database: bool,
    },
}

/// What happened, for a caller to render.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Report {
    pub slug: String,
    /// Already inactive, so a deactivate changed nothing.
    pub no_change: bool,
    pub deactivated: bool,
    pub schema_dropped: Option<String>,
    pub database_dropped: Option<String>,
    pub row_deleted: bool,
    /// Media objects deleted from storage before the purge.
    pub media_deleted: usize,
    /// `(disk, key)` of each media object left in storage: no disk
    /// configured, or a failed delete. Unknowable once the rows are gone.
    pub media_left: Vec<(String, String)>,
    /// Anything the operator still has to do by hand.
    pub notes: Vec<String>,
}

/// Take `slug` out of service.
///
/// Confirmation is the caller's job — a CLI prompt, a typed slug in a
/// form. By the time this is called the decision is made.
///
/// # Errors
/// No such tenant, a schema-mode tenant on a non-Postgres registry, a
/// database-mode purge without `purge_database`, or a driver failure.
pub async fn decommission<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    action: Action,
) -> Result<Report, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(&registry)
        .await?;
    let org = rows
        .into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("no tenant with slug `{slug}`")))?;

    let mut report = Report {
        slug: slug.to_owned(),
        ..Report::default()
    };

    match action {
        Action::Deactivate => {
            if !org.active {
                report.no_change = true;
                return Ok(report);
            }
            let id = org
                .id
                .get()
                .copied()
                .ok_or_else(|| TenancyError::Validation("Org row has no PK".into()))?;
            if deactivate(&registry, id).await? == 0 {
                return Err(TenancyError::Validation(format!(
                    "no row updated for id {id} — race condition?"
                )));
            }
            super::invalidate_org_cache();
            report.deactivated = true;
        }
        Action::Purge { purge_database } => {
            let rep = OpReporter::new(None);
            purge(pools, &registry, &org, purge_database, &mut report, &rep).await?;
        }
    }
    Ok(report)
}

/// Purge `slug`, reporting each step to `observer` and recording a run of
/// kind [`RunKind::Purge`], as the console's run page and `purge-tenant` show.
///
/// # Errors
/// As [`decommission`] with [`Action::Purge`]; the run is closed either way.
pub async fn purge_recorded<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    purge_database: bool,
    observer: Option<&dyn OperationObserver>,
    requested_by: Option<&str>,
) -> Result<(super::provision_store::ProvisioningRun, Report), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use super::provision_store as store;
    let registry = pools.registry_pool();
    let org = tenant(&registry, slug).await?;
    let run = store::open_operation_run(&registry, RunKind::Purge, &org, requested_by).await?;
    let run_id = run.id.get().copied().unwrap_or_default();
    let report = purge_org_in_run(pools, &org, purge_database, observer, run_id).await;
    let run = store::run_by_id(&registry, run_id).await?.unwrap_or(run);
    report.map(|r| (run, r))
}

/// [`purge_recorded`] into a run the caller opened.
///
/// # Errors
/// As [`purge_recorded`].
pub async fn purge_in_run<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    purge_database: bool,
    observer: Option<&dyn OperationObserver>,
    run_id: i64,
) -> Result<Report, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    match tenant(&registry, slug).await {
        Ok(org) => purge_org_in_run(pools, &org, purge_database, observer, run_id).await,
        Err(e) => {
            let rep = OpReporter::new(observer).persisting(&registry, run_id, slug);
            let failed = rep.fail(Step::Check, e).await;
            rep.finish(failed.as_ref().err()).await;
            failed
        }
    }
}

async fn purge_org_in_run<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    purge_database: bool,
    observer: Option<&dyn OperationObserver>,
    run_id: i64,
) -> Result<Report, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let rep = OpReporter::new(observer).persisting(&registry, run_id, &org.slug);
    let mut report = Report {
        slug: org.slug.clone(),
        ..Report::default()
    };
    let result = purge(pools, &registry, org, purge_database, &mut report, &rep).await;
    rep.finish(result.as_ref().err()).await;
    result.map(|()| report)
}

async fn tenant(registry: &crate::sql::Pool, slug: &str) -> Result<Org, TenancyError> {
    Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .first(registry)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("no tenant with slug `{slug}`")))
}

/// `active = false` and unlink its unsucceeded runs, in one transaction:
/// it was activated, so no provisioning retry may revive it (#2292).
async fn deactivate(registry: &crate::sql::Pool, id: i64) -> Result<u64, TenancyError> {
    let update = Org::objects()
        .where_(Org::id.eq(id))
        .update()
        .set("active", false)
        .compile()
        .map_err(crate::sql::ExecError::from)?;
    let mut tx = crate::sql::write_transaction(registry).await?;
    let updated = crate::sql::update_tx(&mut tx, &update).await?;
    if updated > 0 {
        let forget = super::provision_store::forget_unsucceeded_runs(id)?;
        crate::sql::update_tx(&mut tx, &forget).await?;
    }
    tx.commit().await?;
    Ok(updated)
}

/// Delete the tenant's media objects, by its `rustango_media` rows: a
/// key can't be found once they are gone (#2569). Only from storage proven
/// to be the tenant's own; best-effort, what stays is listed in the report.
#[cfg(feature = "media")]
async fn purge_media<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    mode: StorageMode,
    pg_exclusive: bool,
    report: &mut Report,
) where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Model as _;
    use crate::media::Media;
    let table = Media::SCHEMA.table;

    let rows: Result<Vec<Media>, String> = async {
        match mode {
            // By name: with the schema gone, the search path reaches `public`.
            StorageMode::Schema => {
                if !schema_has_table(registry, org.effective_schema(), table).await? {
                    return Ok(Vec::new());
                }
            }
            // The Postgres guards above already proved it exclusive.
            StorageMode::Database if pg_exclusive => {}
            StorageMode::Database => exclusive_database(pools, registry, org).await?,
        }
        let (pool, own) = pools
            .existing_pool_dyn(org)
            .await
            .map_err(|e| e.to_string())?;
        let rows = async {
            // Defence in depth: the registry's own table catches a twin the identity missed.
            if mode == StorageMode::Database
                && !pg_exclusive
                && crate::migrate::try_table_exists_here(&pool, Org::SCHEMA.table)
                    .await
                    .map_err(|e| e.to_string())?
            {
                return Err("the database may be shared: it holds the registry's tables".into());
            }
            if mode == StorageMode::Database
                && !crate::migrate::try_table_exists_here(&pool, table)
                    .await
                    .map_err(|e| e.to_string())?
            {
                return Ok(Vec::new());
            }
            Media::objects()
                .fetch(&pool)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        if own {
            pool.close().await;
        }
        rows
    }
    .await;
    // The listing may have opened a tenant pool; don't keep it past the drop.
    pools.invalidate(&org.slug).await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            report.notes.push(format!(
                "media objects not deleted, any stay in storage: {e}"
            ));
            return;
        }
    };
    let storage = pools.pool_config().media_storage.as_ref();
    let mut first_error = None;
    for m in rows {
        let deleted = match storage.map(|s| s.disk(&m.disk)) {
            Some(Some(disk)) => disk.delete(&m.storage_key).await.map_err(|e| e.to_string()),
            Some(None) => Err(format!("unknown disk `{}`", m.disk)),
            None => Err("no `TenantPoolsConfig::media_storage` set".to_owned()),
        };
        match deleted {
            Ok(()) => report.media_deleted += 1,
            Err(e) => {
                report.media_left.push((m.disk, m.storage_key));
                first_error.get_or_insert(e);
            }
        }
    }
    if let Some(e) = first_error {
        report.notes.push(format!(
            "{} media objects left in storage ({e}); their keys are listed",
            report.media_left.len()
        ));
    }
}

/// Whether `schema.table` exists, asked by name.
#[cfg(feature = "media")]
async fn schema_has_table(
    registry: &crate::sql::Pool,
    schema: &str,
    table: &str,
) -> Result<bool, String> {
    #[allow(unreachable_patterns)]
    match registry {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            crate::migrate::try_table_exists_in_schema(pg, schema, table)
                .await
                .map_err(|e| e.to_string())
        }
        _ => {
            let _ = (schema, table);
            Err("schema mode needs a Postgres registry".to_owned())
        }
    }
}

/// `Ok` when no other tenant and not the registry use this database-mode
/// tenant's database. The Postgres purge refuses that; for a brought
/// database elsewhere only the media step needs the proof.
#[cfg(feature = "media")]
async fn exclusive_database<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
) -> Result<(), String>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let url = pools
        .resolved_database_url(org)
        .await
        .map_err(|e| e.to_string())?;
    let me = DbIdentity::of_url(&url).await?.ok_or_else(|| {
        "the database may be shared: its URL names no existing file or server database".to_owned()
    })?;
    refuse_twins(pools, registry, org, &me, false).await
}

/// `Ok` when neither the registry nor another tenant uses database `me`.
/// `lenient` only logs what cannot be read: a brought Postgres database
/// used to be checked by name alone.
async fn refuse_twins<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    me: &DbIdentity,
    lenient: bool,
) -> Result<(), String>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let shared = |why: String| format!("the database may be shared: {why}");
    let unread = |who: &str, e: String| {
        tracing::warn!(target: "rustango::tenancy", slug = %org.slug, error = %e, "{who}'s database identity is unknown; not compared");
    };
    match DbIdentity::of_pool(registry).await {
        Ok(id) if id.as_ref() == Some(me) => return Err(shared("it is the registry's".into())),
        Ok(_) => {}
        Err(e) if lenient => unread("the registry", e),
        Err(e) => return Err(e),
    }
    let others: Vec<Org> = Org::objects()
        .where_(Org::storage_mode.eq(StorageMode::Database.as_str().to_owned()))
        .fetch(registry)
        .await
        .map_err(|e| e.to_string())?;
    for other in others
        .iter()
        .filter(|o| o.id.get() != org.id.get() && o.database_url.is_some())
    {
        let cant = |e: String| shared(format!("tenant `{}` can't be checked: {e}", other.slug));
        let url = pools
            .resolved_database_url(other)
            .await
            .map_err(|e| cant(e.to_string()))?;
        if !me.same_backend(&url) {
            continue;
        }
        match DbIdentity::of_url(&url).await {
            Ok(id) if id.as_ref() == Some(me) => {
                return Err(shared(format!("tenant `{}` uses it too", other.slug)))
            }
            Ok(_) => {}
            Err(e) if lenient => unread(&format!("tenant `{}`", other.slug), e),
            Err(e) => return Err(cant(e)),
        }
    }
    Ok(())
}

/// How a database-mode tenant's database goes, decided before anything
/// changes.
enum DropPlan {
    /// No database to drop (schema mode).
    None,
    /// Made by rustango on a saved server: dropped by that server's admin.
    Server(ServerDrop),
    /// Brought by the operator, on Postgres: dropped through its own URL.
    #[cfg(feature = "postgres")]
    Brought(PgDropTarget),
    /// Brought, elsewhere: left in place.
    Left,
}

/// A recorded tenant database, checked and with its admin connected.
struct ServerDrop {
    tenant: ServerTenant,
    admin: ServerAdmin,
    /// The `application_name` this tenant's pools connect with.
    tag: String,
}

async fn purge<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    purge_database: bool,
    report: &mut Report,
    rep: &OpReporter<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    // Every refusal before the org is touched, so a refused purge leaves it active.
    rep.started(Step::Check).await;
    let (mode, id, plan) = match check_purge(pools, registry, org, purge_database).await {
        Ok(checked) => checked,
        Err(e) => return rep.fail(Step::Check, e).await,
    };
    rep.ok(Step::Check).await;
    let mut plan = plan;
    let result = destroy(pools, registry, org, mode, id, &mut plan, report, rep).await;
    if let DropPlan::Server(s) = plan {
        s.admin.close().await;
    }
    result
}

async fn check_purge<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    purge_database: bool,
) -> Result<(StorageMode, i64, DropPlan), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let slug = &org.slug;
    let mode = StorageMode::parse(&org.storage_mode)
        .map_err(|e| TenancyError::Validation(e.to_owned()))?;
    if mode == StorageMode::Database && !purge_database {
        return Err(TenancyError::Validation(format!(
            "tenant `{slug}` is database-mode — dropping its database is \
             unrecoverable. Ask for the database to be purged explicitly, or \
             deactivate instead."
        )));
    }
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("Org row has no PK".into()))?;
    if mode == StorageMode::Schema {
        refuse_shared_schema(registry, org.effective_schema(), id).await?;
        return Ok((mode, id, DropPlan::None));
    }
    let url = pools.resolved_database_url(org).await?;
    // A record never makes the registry's own database droppable.
    #[cfg(feature = "postgres")]
    if org.backend_kind == "postgres" {
        refuse_registry_database(pools, &parse_pg_url(org, &url)?)?;
    }
    if let Some(tenant) = ServerTenant::of(registry, org, &url).await? {
        let plan = check_server_drop(pools, registry, org, tenant).await?;
        return Ok((mode, id, DropPlan::Server(plan)));
    }
    #[cfg(feature = "postgres")]
    if org.backend_kind == "postgres" {
        let opts = parse_pg_url(org, &url)?;
        if let Some(other) = database_claimed(pools, registry, id, &opts).await? {
            return Err(TenancyError::Validation(format!(
                "refusing to drop this tenant's database — tenant `{other}` uses it too"
            )));
        }
        let mut target = PgDropTarget::connect(&opts, super::pools::tenant_session_tag(id)).await?;
        target.refuse_foreign_sessions().await?;
        // An idle alias holds no session and spells the host its own way.
        if let Ok(Some(me)) = DbIdentity::of_url(&url).await {
            refuse_twins(pools, registry, org, &me, true)
                .await
                .map_err(|why| {
                    TenancyError::Validation(format!(
                        "refusing to drop this tenant's database — {why}"
                    ))
                })?;
        }
        return Ok((mode, id, DropPlan::Brought(target)));
    }
    Ok((mode, id, DropPlan::Left))
}

/// The refusals for a database rustango made: no other tenant names it,
/// the admin may drop it and its login, and no foreign session is open.
async fn check_server_drop<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    tenant: ServerTenant,
) -> Result<ServerDrop, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let id = org.id.get().copied().unwrap_or_default();
    let backend = tenant.server.backend_kind()?;
    let others: Vec<Org> = Org::objects()
        .where_(Org::storage_mode.eq(StorageMode::Database.as_str().to_owned()))
        .fetch(registry)
        .await?;
    for other in others.iter().filter(|o| o.id.get().copied() != Some(id)) {
        let url = pools.resolved_database_url(other).await.map_err(|_| {
            TenancyError::Validation(format!(
                "cannot resolve tenant `{}`'s database URL to rule out a shared database",
                other.slug
            ))
        })?;
        match tenant
            .server
            .may_name_database(backend, &url, tenant.database.as_str())
        {
            Ok(false) => {}
            Ok(true) => {
                return Err(TenancyError::Validation(format!(
                    "refusing to drop this tenant's database — tenant `{}` uses it too",
                    other.slug
                )))
            }
            Err(why) => {
                return Err(TenancyError::Validation(format!(
                    "refusing to drop this tenant's database — tenant `{}` may use it ({why})",
                    other.slug
                )))
            }
        }
    }
    let admin = tenant.connect().await?;
    let tag = super::pools::tenant_session_tag(id);
    let checked = async {
        let report = admin.probe().await?;
        if let Some(why) = report.login_error.as_ref().map(|d| d.summary()) {
            return Err(TenancyError::Validation(format!(
                "the admin login of server `{}` is refused: {why}",
                tenant.server.name
            )));
        }
        // MySQL has no per-object check: the global rights decide, before media goes.
        if report.backend == "mysql" && !(report.can_create_database && report.can_create_user) {
            return Err(TenancyError::Validation(format!(
                "the admin login of server `{}` may not drop databases and logins (it needs \
                 global DROP and CREATE USER); grant them and purge again",
                tenant.server.name
            )));
        }
        // An alias, a twin or the registry: names alone can't tell on these backends.
        if backend != super::org::BackendKind::Postgres {
            let me = server_identity(&admin, &tenant)
                .await
                .map_err(TenancyError::Validation)?;
            if let Some(me) = me {
                refuse_twins(pools, registry, org, &me, false)
                    .await
                    .map_err(|why| {
                        TenancyError::Validation(format!(
                            "refusing to drop this tenant's database — {why}"
                        ))
                    })?;
            }
        }
        let rights = admin
            .tenant_rights(&tenant.database, tenant.login.as_ref())
            .await?;
        if !rights.drop_database {
            return Err(TenancyError::Validation(format!(
                "the admin login of server `{}` does not own database `{}` and is not a \
                 superuser, so it may not drop it; drop it as its owner or a superuser",
                tenant.server.name, tenant.database
            )));
        }
        if let (false, Some(login)) = (rights.alter_login, &tenant.login) {
            return Err(super::server_tenant::admin_option_refusal(
                login.name(),
                &tenant.server.name,
            ));
        }
        let sessions = admin.sessions(&tenant.database, &tag).await?;
        if sessions.foreign > 0 {
            return Err(TenancyError::Validation(format!(
                "database `{}` has sessions this tenant's pools did not open (an operator, a \
                 backup or another app); close them and purge again",
                tenant.database
            )));
        }
        if sessions.own > 0 && !rights.end_sessions {
            return Err(TenancyError::Validation(format!(
                "the admin login of server `{}` may not end this tenant's open sessions; grant \
                 it pg_signal_backend, or stop the app, and purge again",
                tenant.server.name
            )));
        }
        Ok(())
    }
    .await;
    match checked {
        Ok(()) => Ok(ServerDrop { tenant, admin, tag }),
        Err(e) => {
            admin.close().await;
            Err(e)
        }
    }
}

/// The identity of a recorded database, read through its server's admin:
/// `None` once a SQLite file is gone, or on Postgres.
async fn server_identity(
    admin: &ServerAdmin,
    tenant: &ServerTenant,
) -> Result<Option<DbIdentity>, String> {
    #[cfg(feature = "sqlite")]
    if let Some(path) = admin.sqlite_path(&tenant.database) {
        return Ok(DbIdentity::file(&path));
    }
    #[cfg(feature = "mysql")]
    if let Some(pool) = admin.server_pool() {
        return DbIdentity::on_server(pool, tenant.database.as_str()).await;
    }
    let _ = (admin, tenant);
    Ok(None)
}

/// Everything after the checks. Each drop is `IF EXISTS`, so a retry
/// after a partial purge is safe.
#[allow(clippy::too_many_arguments)]
async fn destroy<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    mode: StorageMode,
    id: i64,
    plan: &mut DropPlan,
    report: &mut Report,
    rep: &OpReporter<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let slug = &org.slug;
    // Out of service before anything is destroyed (#1930): if a later
    // step fails, the tenant is inactive and a retry finishes the job.
    rep.started(Step::Deactivate).await;
    if let Err(e) = deactivate(registry, id).await {
        return rep.fail(Step::Deactivate, e).await;
    }
    super::invalidate_org_cache();
    pools.invalidate(slug).await;
    rep.ok(Step::Deactivate).await;

    // While the rows that name the keys still exist (#2569).
    #[cfg(feature = "media")]
    {
        rep.started(Step::PurgeMedia).await;
        #[cfg(feature = "postgres")]
        let brought_pg = matches!(plan, DropPlan::Brought(_));
        #[cfg(not(feature = "postgres"))]
        let brought_pg = false;
        let exclusive = brought_pg || matches!(plan, DropPlan::Server(_));
        purge_media(pools, registry, org, mode, exclusive, report).await;
        rep.ok(Step::PurgeMedia).await;
    }
    #[cfg(not(feature = "media"))]
    rep.skipped(Step::PurgeMedia, "this build has no media feature")
        .await;

    match plan {
        DropPlan::None => {
            rep.started(Step::DropSchema).await;
            let schema = org.effective_schema().to_owned();
            // Quoted: the name is an identifier, and validation only
            // started covering it recently — older rows may hold
            // anything.
            let sql = format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                registry.dialect().quote_ident(&schema)
            );
            if let Err(e) = crate::sql::raw_execute_pool(registry, &sql, Vec::new()).await {
                return rep.fail(Step::DropSchema, e.into()).await;
            }
            report.schema_dropped = Some(schema);
            rep.ok(Step::DropSchema).await;
        }
        DropPlan::Server(s) => drop_server_database(s, report, rep).await?,
        #[cfg(feature = "postgres")]
        DropPlan::Brought(target) => {
            rep.started(Step::DropDatabase).await;
            if let Err(e) = target.drop().await {
                return rep.fail(Step::DropDatabase, e).await;
            }
            let url = pools.resolved_database_url(org).await.unwrap_or_default();
            report.database_dropped = Some(crate::sql::connect_diagnosis::redact(&url));
            rep.ok(Step::DropDatabase).await;
            leave_login_and_record(rep).await;
        }
        DropPlan::Left => {
            let url = pools.resolved_database_url(org).await?;
            report.notes.push(format!(
                "delete the tenant database at {} by hand — rustango did not create it, \
                 and dropping a brought `{}` database is not wired up",
                crate::sql::connect_diagnosis::redact(&url),
                org.backend_kind
            ));
            rep.skipped(
                Step::DropDatabase,
                "rustango did not create this database; delete it by hand",
            )
            .await;
            leave_login_and_record(rep).await;
        }
    }
    #[cfg(not(feature = "media"))]
    let _ = mode;

    rep.started(Step::RemoveOrg).await;
    if let Err(e) = remove_org_row(registry, org).await {
        return rep.fail(Step::RemoveOrg, e).await;
    }
    report.row_deleted = true;
    rep.ok(Step::RemoveOrg).await;

    // Last: while the org stays, a retry finds the record and goes by the
    // server's admin, not the tenant login it already dropped.
    if let DropPlan::Server(s) = plan {
        rep.started(Step::DeleteRecord).await;
        if let Err(e) = s.tenant.record.clone().delete_pool(registry).await {
            return rep.fail(Step::DeleteRecord, e.into()).await;
        }
        rep.ok(Step::DeleteRecord).await;
    }
    Ok(())
}

async fn leave_login_and_record(rep: &OpReporter<'_>) {
    rep.skipped(
        Step::DropLogin,
        "rustango did not create the login; it is left alone",
    )
    .await;
    rep.skipped(Step::DeleteRecord, "rustango did not create this database")
        .await;
}

/// Drop a recorded database, then its login: sessions tagged as this
/// tenant's pools, on any pod, are ended first. The record goes last.
async fn drop_server_database(
    s: &ServerDrop,
    report: &mut Report,
    rep: &OpReporter<'_>,
) -> Result<(), TenancyError> {
    let t = &s.tenant;
    let login = t.login.as_ref().map(|l| l.name());
    rep.started(Step::DropDatabase).await;
    if let Err(e) = s.admin.drop_database(&t.database, Some(&s.tag)).await {
        return rep.fail(Step::DropDatabase, e.into()).await;
    }
    report.database_dropped = Some(format!(
        "database `{}` on server `{}`",
        t.database, t.server.name
    ));
    rep.done(
        Step::DropDatabase,
        RunText::about_database("dropped", &t.database, None),
    )
    .await;

    match &t.login {
        Some(l) => {
            rep.started(Step::DropLogin).await;
            if let Err(e) = s.admin.drop_login(l).await {
                return rep.fail(Step::DropLogin, e.into()).await;
            }
            rep.done(
                Step::DropLogin,
                RunText::about_database("dropped", &t.database, login),
            )
            .await;
        }
        None => {
            rep.skipped(Step::DropLogin, "a SQLite tenant has no login")
                .await
        }
    }
    Ok(())
}

/// Delete `org`'s row, its extra hosts first, and unlink its runs.
pub(crate) async fn remove_org_row(
    registry: &crate::sql::Pool,
    org: &Org,
) -> Result<(), TenancyError> {
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("Org row has no PK".into()))?;
    // Extra hosts first: their FK has no ON DELETE, so the Org delete
    // fails while any remain (#1930).
    let hosts = OrgHost::objects()
        .where_(OrgHost::org_id.eq(id))
        .compile_delete()
        .map_err(crate::sql::ExecError::from)?;
    crate::sql::delete_pool(registry, &hosts).await?;
    super::invalidate_host_cache();
    // Runs keep their history but forget the Org, so a later Org that
    // reuses the id cannot look like the one they left half-made.
    super::provision_store::ProvisioningRun::objects()
        .where_(super::provision_store::ProvisioningRun::org_id.eq(Some(id)))
        .update()
        .set("org_id", None::<i64>)
        .execute_pool(registry)
        .await?;
    let deleted = org.clone().delete_pool(registry).await?;
    if deleted == 0 {
        return Err(TenancyError::Validation(format!(
            "no Org row deleted for id {id} — race condition?"
        )));
    }
    super::invalidate_org_cache();
    Ok(())
}

/// Never `DROP SCHEMA … CASCADE` a reserved schema or one another tenant
/// still uses: rows made before #2290 may share one.
async fn refuse_shared_schema(
    registry: &crate::sql::Pool,
    schema: &str,
    org_id: i64,
) -> Result<(), TenancyError> {
    if let Err(why) = super::provision::validate_schema_name(schema) {
        return Err(TenancyError::Validation(format!(
            "refusing to drop schema `{schema}`: {why}"
        )));
    }
    if super::org_host::schema_claimed(registry, schema, Some(org_id)).await? {
        return Err(TenancyError::Validation(format!(
            "refusing to drop schema `{schema}` — another tenant uses it too"
        )));
    }
    let outside = outside_dependents(registry, schema).await?;
    if outside.is_empty() {
        return Ok(());
    }
    let mut extensions: Vec<String> = Vec::new();
    for (_, _, ext, relocatable) in &outside {
        let Some(ext) = ext else { continue };
        let advice = if *relocatable == Some(true) {
            format!("move extension {ext} with `ALTER EXTENSION {ext} SET SCHEMA public`")
        } else {
            format!("extension {ext} cannot move: drop it and reinstall it in another schema")
        };
        if !extensions.contains(&advice) {
            extensions.push(advice);
        }
    }
    let listed: Vec<String> = outside
        .iter()
        .map(|(dep, uses, _, _)| format!("{dep} (uses {uses})"))
        .collect();
    let mut msg = format!(
        "refusing to drop schema `{schema}` — the CASCADE would drop objects outside it: {}; \
         drop or move them first",
        listed.join(", ")
    );
    for advice in extensions {
        msg.push_str("; ");
        msg.push_str(&advice);
    }
    Err(TenancyError::Validation(msg))
}

/// Objects outside `$1` that `DROP SCHEMA $1 CASCADE` would drop, each with
/// the object inside it uses and that object's extension, if any.
/// Namespaces compare by OID: catalog names come back quoted (`"a-b"`).
#[cfg(feature = "postgres")]
const OUTSIDE_DEPENDENTS: &str = "WITH RECURSIVE target AS ( \
        SELECT n.oid FROM pg_namespace n WHERE n.nspname = $1::text \
    ), dropped(classid, objid, objsubid, pclassid, pobjid, psubid) AS ( \
        SELECT 'pg_namespace'::regclass::oid, t.oid, 0, 0::oid, 0::oid, 0 FROM target t \
      UNION \
        SELECT d.classid, d.objid, d.objsubid, d.refclassid, d.refobjid, d.refobjsubid \
          FROM pg_depend d JOIN dropped p ON d.refclassid = p.classid \
           AND d.refobjid = p.objid AND (p.objsubid = 0 OR d.refobjsubid = p.objsubid) \
         WHERE d.deptype IN ('n', 'a', 'i', 'e') \
    ), placed AS ( \
        SELECT x.*, COALESCE(to_regnamespace(i.schema)::oid, (SELECT c.relnamespace \
            FROM pg_class c WHERE c.oid = CASE x.classid \
              WHEN 'pg_rewrite'::regclass THEN (SELECT ev_class FROM pg_rewrite WHERE oid = x.objid) \
              WHEN 'pg_attrdef'::regclass THEN (SELECT adrelid FROM pg_attrdef WHERE oid = x.objid) \
              WHEN 'pg_trigger'::regclass THEN (SELECT tgrelid FROM pg_trigger WHERE oid = x.objid) \
              WHEN 'pg_policy'::regclass THEN (SELECT polrelid FROM pg_policy WHERE oid = x.objid) \
            END)) AS nsp \
          FROM dropped x, LATERAL pg_identify_object(x.classid, x.objid, x.objsubid) i \
    ) \
    SELECT DISTINCT pg_describe_object(o.classid, o.objid, o.objsubid), \
           pg_describe_object(o.pclassid, o.pobjid, o.psubid), e.extname::text, e.extrelocatable \
      FROM placed o \
      JOIN placed p ON p.classid = o.pclassid AND p.objid = o.pobjid \
       AND p.objsubid IN (0, o.psubid) \
      LEFT JOIN pg_depend m ON m.classid = o.pclassid AND m.objid = o.pobjid AND m.deptype = 'e' \
      LEFT JOIN pg_extension e ON e.oid = m.refobjid \
     CROSS JOIN target t \
     WHERE o.nsp NOT IN (t.oid, 'pg_toast'::regnamespace) AND (p.nsp IS NULL OR p.nsp = t.oid) \
     ORDER BY 1, 2";

/// Catalog query: schema mode is Postgres-only.
async fn outside_dependents(
    registry: &crate::sql::Pool,
    schema: &str,
) -> Result<Vec<(String, String, Option<String>, Option<bool>)>, TenancyError> {
    #[allow(unreachable_patterns)]
    match registry {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => Ok(crate::sql::sqlx::query_as(OUTSIDE_DEPENDENTS)
            .bind(schema)
            .fetch_all(pg)
            .await?),
        _ => {
            let _ = schema;
            Err(TenancyError::Validation(
                "schema mode needs a Postgres registry".into(),
            ))
        }
    }
}

/// Same Postgres database: host (case-insensitive), port and name.
/// Text-level, so `localhost` and `127.0.0.1` differ; the session check covers that.
#[cfg(feature = "postgres")]
fn same_pg_database(
    a: &crate::sql::sqlx::postgres::PgConnectOptions,
    b: &crate::sql::sqlx::postgres::PgConnectOptions,
) -> bool {
    a.get_host().eq_ignore_ascii_case(b.get_host())
        && a.get_port() == b.get_port()
        && a.get_database() == b.get_database()
}

#[cfg(feature = "postgres")]
/// `url` is `org`'s resolved URL. The error names the stored reference:
/// a resolved secret need not look like a URL, so `redact` cannot mask it.
fn parse_pg_url(
    org: &Org,
    url: &str,
) -> Result<crate::sql::sqlx::postgres::PgConnectOptions, TenancyError> {
    use std::str::FromStr;
    crate::sql::sqlx::postgres::PgConnectOptions::from_str(url).map_err(|_| {
        TenancyError::Validation(format!(
            "cannot parse tenant `{}`'s database URL ({})",
            org.slug,
            super::secrets::shown_reference(org.database_url.as_deref().unwrap_or_default())
        ))
    })
}

/// An early, friendly refusal when the URL names the registry pool's database.
#[cfg(feature = "postgres")]
fn refuse_registry_database<DB: Database>(
    pools: &TenantPools<DB>,
    tenant: &crate::sql::sqlx::postgres::PgConnectOptions,
) -> Result<(), TenancyError> {
    let Some(pg) = (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
    else {
        return Ok(());
    };
    if same_pg_database(tenant, &pg.registry_inner().connect_options()) {
        return Err(TenancyError::Validation(
            "refusing to drop this tenant's database — it is the registry's own".into(),
        ));
    }
    Ok(())
}

/// Another Postgres database-mode tenant points at the same database. An idle
/// one holds no session, so only the registry rows can tell (#2291).
#[cfg(feature = "postgres")]
async fn database_claimed<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org_id: i64,
    tenant: &crate::sql::sqlx::postgres::PgConnectOptions,
) -> Result<Option<String>, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let others: Vec<Org> = Org::objects()
        .where_(Org::storage_mode.eq(StorageMode::Database.as_str().to_owned()))
        .where_(Org::backend_kind.eq("postgres".to_owned()))
        .fetch(registry)
        .await?;
    for other in others
        .iter()
        .filter(|o| o.id.get().copied() != Some(org_id))
    {
        // An unresolvable or unparsable URL may well be this database.
        let url = pools.resolved_database_url(other).await.map_err(|_| {
            TenancyError::Validation(format!(
                "cannot resolve tenant `{}`'s database URL to rule out a shared database",
                other.slug
            ))
        })?;
        if same_pg_database(tenant, &parse_pg_url(other, &url)?) {
            return Ok(Some(other.slug.clone()));
        }
    }
    Ok(None)
}

/// An admin connection to the server holding a tenant database about to be
/// dropped. Built and checked before the org is touched.
#[cfg(feature = "postgres")]
struct PgDropTarget {
    admin: crate::sql::sqlx::PgConnection,
    dbname: String,
    /// The `application_name` this tenant's pools connect with.
    tag: String,
}

#[cfg(feature = "postgres")]
impl PgDropTarget {
    async fn connect(
        opts: &crate::sql::sqlx::postgres::PgConnectOptions,
        tag: String,
    ) -> Result<Self, TenancyError> {
        use crate::sql::sqlx::ConnectOptions;
        let dbname = opts.get_database().ok_or_else(|| {
            TenancyError::Validation(
                "the tenant's database URL names no database — nothing to drop".into(),
            )
        })?;
        if crate::sql::server::PG_SYSTEM_DATABASES.contains(&dbname.to_ascii_lowercase().as_str()) {
            return Err(TenancyError::Validation(format!(
                "refusing to drop `{dbname}` — that is a Postgres system database"
            )));
        }
        let dbname = dbname.to_owned();
        // `DROP DATABASE` cannot run from inside the database being
        // dropped, so this connects to `postgres` on the same server.
        let admin = opts
            .clone()
            .database(crate::sql::server::PG_MAINTENANCE_DATABASE)
            .connect()
            .await?;
        Ok(Self { admin, dbname, tag })
    }

    fn in_use(&self) -> TenancyError {
        TenancyError::Validation(format!(
            "database `{}` has sessions this tenant's pools did not open \
             (another tenant, the registry or an operator); close them and purge again",
            self.dbname
        ))
    }

    /// Refuse while any session not tagged as this tenant is open.
    async fn refuse_foreign_sessions(&mut self) -> Result<(), TenancyError> {
        let foreign: i64 = crate::sql::sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = $1 AND application_name IS DISTINCT FROM $2",
        )
        .bind(&self.dbname)
        .bind(&self.tag)
        .fetch_one(&mut self.admin)
        .await?;
        if foreign > 0 {
            return Err(self.in_use());
        }
        Ok(())
    }

    /// End this tenant's own sessions, on any pod, then a plain drop: a
    /// session that appeared since the check still blocks it (#2291).
    async fn drop(&mut self) -> Result<(), TenancyError> {
        crate::sql::sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND application_name = $2",
        )
        .bind(&self.dbname)
        .bind(&self.tag)
        .execute(&mut self.admin)
        .await?;
        // Postgres by construction — the caller checks `backend_kind`.
        let sql = format!(
            "DROP DATABASE IF EXISTS \"{}\"",
            self.dbname.replace('"', "\"\"")
        );
        match crate::sql::sqlx::query(&sql).execute(&mut self.admin).await {
            Ok(_) => Ok(()),
            // 55006 object_in_use.
            Err(crate::sql::sqlx::Error::Database(e)) if e.code().as_deref() == Some("55006") => {
                Err(self.in_use())
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::*;

    /// A reference that resolves to a bare secret is named, never echoed.
    #[test]
    fn an_unparsable_url_names_the_reference_not_the_secret() {
        let org = Org {
            slug: "acme".into(),
            database_url: Some("env://ACME_DB".into()),
            ..crate::testkit::org()
        };
        let err = parse_pg_url(&org, "hunter2-leak").unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "{err}");
        assert!(
            err.contains("env://ACME_DB") && err.contains("acme"),
            "{err}"
        );
    }
}
