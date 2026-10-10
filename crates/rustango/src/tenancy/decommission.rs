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
    /// Media objects left in storage (no disk configured, or a failed delete).
    pub media_left: usize,
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
    let mut tx = crate::sql::write_transaction_pool(registry).await?;
    let updated = crate::sql::update_tx(&mut tx, &update).await?;
    if updated > 0 {
        let forget = super::provision_store::forget_unsucceeded_runs(id)?;
        crate::sql::update_tx(&mut tx, &forget).await?;
    }
    tx.commit().await?;
    Ok(updated)
}

/// Delete the tenant's media objects, by its `rustango_media` rows: a
/// key can't be found once they are gone (#2569). Best-effort; what
/// stays is counted and noted.
#[cfg(feature = "media")]
async fn purge_media<DB: Database>(pools: &TenantPools<DB>, org: &Org, report: &mut Report)
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Model as _;
    use crate::media::Media;

    let rows: Result<Vec<Media>, String> = async {
        let pool = pools
            .scoped_pool_dyn(org)
            .await
            .map_err(|e| e.to_string())?;
        let has_table = crate::migrate::try_table_exists_here(&pool, Media::SCHEMA.table)
            .await
            .map_err(|e| e.to_string())?;
        if !has_table {
            return Ok(Vec::new());
        }
        Media::objects()
            .fetch(&pool)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    // The listing opened a tenant pool; don't keep it past the drop.
    pools.invalidate(&org.slug).await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            report.notes.push(format!(
                "media objects not checked, any stay in storage: {e}"
            ));
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    let Some(storage) = pools.pool_config().media_storage.as_ref() else {
        report.media_left = rows.len();
        report.notes.push(format!(
            "{} media objects left in storage: set `TenantPoolsConfig::media_storage` to delete them",
            rows.len()
        ));
        return;
    };
    let mut first_error = None;
    for m in &rows {
        let deleted = match storage.disk(&m.disk) {
            Some(disk) => disk.delete(&m.storage_key).await.map_err(|e| e.to_string()),
            None => Err(format!("unknown disk `{}`", m.disk)),
        };
        match deleted {
            Ok(()) => report.media_deleted += 1,
            Err(e) => {
                report.media_left += 1;
                first_error.get_or_insert(e);
            }
        }
    }
    if let Some(e) = first_error {
        report.notes.push(format!(
            "{} media objects left in storage (first error: {e})",
            report.media_left
        ));
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
        let opts = parse_pg_url(&pools.resolved_database_url(org).await?)?;
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
    purge_media(pools, org, report).await;

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
    report.row_deleted = true;
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
    Ok(())
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
fn parse_pg_url(url: &str) -> Result<crate::sql::sqlx::postgres::PgConnectOptions, TenancyError> {
    use std::str::FromStr;
    crate::sql::sqlx::postgres::PgConnectOptions::from_str(url).map_err(|e| {
        TenancyError::Validation(format!(
            "cannot parse the tenant's database URL: {e} ({})",
            crate::sql::connect_diagnosis::redact(url)
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
        if same_pg_database(tenant, &parse_pg_url(&url)?) {
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
        if matches!(
            dbname.to_ascii_lowercase().as_str(),
            "postgres" | "template0" | "template1"
        ) {
            return Err(TenancyError::Validation(format!(
                "refusing to drop `{dbname}` — that is a Postgres system database"
            )));
        }
        let dbname = dbname.to_owned();
        // `DROP DATABASE` cannot run from inside the database being
        // dropped, so this connects to `postgres` on the same server.
        let admin = opts.clone().database("postgres").connect().await?;
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
