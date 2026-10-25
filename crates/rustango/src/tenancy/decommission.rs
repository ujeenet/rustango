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

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::org_host::OrgHost;
use super::pools::TenantPools;
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
            purge(pools, &registry, &org, purge_database, &mut report).await?;
        }
    }
    Ok(report)
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
/// tenant's database. The Postgres purge refuses that; other backends drop
/// nothing, so only the media step needs the proof (#2569).
#[cfg(feature = "media")]
async fn exclusive_database<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
) -> Result<(), String>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let shared = |why: String| format!("the database may be shared: {why}");
    let url = pools
        .resolved_database_url(org)
        .await
        .map_err(|e| e.to_string())?;
    let me = DbIdentity::of_url(&url)
        .await?
        .ok_or_else(|| shared("its URL names no existing file or server database".into()))?;
    if DbIdentity::of_pool(registry).await?.as_ref() == Some(&me) {
        return Err(shared("it is the registry's".into()));
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
        if DbIdentity::of_url(&url).await.map_err(cant)?.as_ref() == Some(&me) {
            return Err(shared(format!("tenant `{}` uses it too", other.slug)));
        }
    }
    Ok(())
}

/// Which database a SQLite or MySQL tenant uses: a canonical file, or the
/// live server's uuid and database, so a host alias can't hide a twin.
#[cfg(feature = "media")]
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(all(feature = "sqlite", feature = "mysql")), allow(dead_code))]
enum DbIdentity {
    File(std::path::PathBuf),
    Server { uuid: String, database: String },
}

#[cfg(feature = "media")]
impl DbIdentity {
    /// `Ok(None)` for another backend or a missing / in-memory SQLite file.
    async fn of_url(url: &str) -> Result<Option<Self>, String> {
        #[cfg(feature = "sqlite")]
        if url.starts_with("sqlite:") {
            let o = <sqlx::sqlite::SqliteConnectOptions as std::str::FromStr>::from_str(url)
                .map_err(|e| e.to_string())?;
            return Ok(Self::file(o.get_filename()));
        }
        #[cfg(feature = "mysql")]
        if url.starts_with("mysql:") || url.starts_with("mariadb:") {
            let pool = crate::sql::Pool::connect(url)
                .await
                .map_err(|e| e.to_string())?;
            let id = Self::of_pool(&pool).await;
            pool.close().await;
            return id;
        }
        let _ = url;
        Ok(None)
    }

    async fn of_pool(pool: &crate::sql::Pool) -> Result<Option<Self>, String> {
        #[allow(unreachable_patterns)]
        match pool {
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(p) => Ok(Self::file(p.connect_options().get_filename())),
            #[cfg(feature = "mysql")]
            crate::sql::Pool::Mysql(p) => Self::server(p).await.map(Some),
            _ => Ok(None),
        }
    }

    fn same_backend(&self, url: &str) -> bool {
        matches!(self, Self::File(_)) == url.starts_with("sqlite:")
    }

    /// An existing file, canonical: `None` for a missing or in-memory one.
    #[cfg(feature = "sqlite")]
    fn file(path: &std::path::Path) -> Option<Self> {
        std::fs::canonicalize(path).ok().map(Self::File)
    }

    /// Raw SQL: server identity has no ORM model. Names fold as the server
    /// folds them (`lower_case_table_names`).
    #[cfg(feature = "mysql")]
    async fn server(pool: &sqlx::MySqlPool) -> Result<Self, String> {
        let (uuid, database): (String, Option<String>) = sqlx::query_as(
            "SELECT CAST(@@server_uuid AS CHAR), \
             CAST(IF(@@lower_case_table_names = 0, DATABASE(), LOWER(DATABASE())) AS CHAR)",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
        let database = database.ok_or("the connection selects no database")?;
        Ok(Self::Server { uuid, database })
    }
}

async fn purge<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    org: &Org,
    purge_database: bool,
    report: &mut Report,
) -> Result<(), TenancyError>
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
    // Every refusal before the org is touched, so a refused purge leaves it active.
    if mode == StorageMode::Schema {
        refuse_shared_schema(registry, org.effective_schema(), id).await?;
    }
    #[cfg(feature = "postgres")]
    let pg_target = if mode == StorageMode::Database && org.backend_kind == "postgres" {
        let opts = parse_pg_url(org, &pools.resolved_database_url(org).await?)?;
        refuse_registry_database(pools, &opts)?;
        if let Some(other) = database_claimed(pools, registry, id, &opts).await? {
            return Err(TenancyError::Validation(format!(
                "refusing to drop this tenant's database — tenant `{other}` uses it too"
            )));
        }
        let mut target = PgDropTarget::connect(&opts, super::pools::tenant_session_tag(id)).await?;
        target.refuse_foreign_sessions().await?;
        Some(target)
    } else {
        None
    };
    // Out of service before anything is destroyed (#1930): if a later
    // step fails, the tenant is inactive and a retry finishes the job.
    deactivate(registry, id).await?;
    super::invalidate_org_cache();
    pools.invalidate(slug).await;
    // While the rows that name the keys still exist (#2569).
    #[cfg(feature = "media")]
    {
        #[cfg(feature = "postgres")]
        let pg_exclusive = pg_target.is_some();
        #[cfg(not(feature = "postgres"))]
        let pg_exclusive = false;
        purge_media(pools, registry, org, mode, pg_exclusive, report).await;
    }

    // Every drop is `IF EXISTS`, so a retry after a partial purge is safe.
    match mode {
        StorageMode::Schema => {
            let schema = org.effective_schema().to_owned();
            // Quoted: the name is an identifier, and validation only
            // started covering it recently — older rows may hold
            // anything.
            let sql = format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                registry.dialect().quote_ident(&schema)
            );
            crate::sql::raw_execute_pool(registry, &sql, Vec::new()).await?;
            report.schema_dropped = Some(schema);
        }
        StorageMode::Database => {
            let url = pools.resolved_database_url(org).await?;
            if org.backend_kind == "postgres" {
                #[cfg(feature = "postgres")]
                if let Some(target) = pg_target {
                    target.drop().await?;
                    report.database_dropped = Some(crate::sql::connect_diagnosis::redact(&url));
                }
                #[cfg(not(feature = "postgres"))]
                report.notes.push(format!(
                    "the tenant database at {} was left in place — this build has no \
                     `postgres` feature",
                    crate::sql::connect_diagnosis::redact(&url)
                ));
            } else {
                report.notes.push(format!(
                    "delete the tenant database at {} by hand — dropping a `{}` database \
                     is not wired up",
                    crate::sql::connect_diagnosis::redact(&url),
                    org.backend_kind
                ));
            }
        }
    }

    remove_org_row(registry, org).await?;
    report.row_deleted = true;
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
    async fn drop(mut self) -> Result<(), TenancyError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference that resolves to a bare secret is named, never echoed.
    #[cfg(feature = "postgres")]
    #[test]
    fn an_unparsable_url_names_the_reference_not_the_secret() {
        let org = Org {
            slug: "acme".into(),
            database_url: Some("env://ACME_DB".into()),
            ..crate::testkit::org()
        };
        let err = parse_pg_url(&org, "hunter2-leak").unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "{err}");
        assert!(err.contains("env://ACME_DB") && err.contains("acme"), "{err}");
    }

    /// The flag is the whole guard for an unrecoverable act, so the two
    /// purge shapes must not compare equal.
    #[test]
    fn purging_a_database_is_a_distinct_action() {
        assert_ne!(
            Action::Purge {
                purge_database: false
            },
            Action::Purge {
                purge_database: true
            }
        );
        assert_ne!(
            Action::Deactivate,
            Action::Purge {
                purge_database: false
            }
        );
    }
}
