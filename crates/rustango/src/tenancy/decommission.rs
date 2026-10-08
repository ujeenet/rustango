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
            let deactivate = Org::objects()
                .where_(Org::id.eq(id))
                .update()
                .set("active", false)
                .compile()
                .map_err(crate::sql::ExecError::from)?;
            let mut tx = crate::sql::write_transaction_pool(&registry).await?;
            let updated = crate::sql::update_tx(&mut tx, &deactivate).await?;
            if updated == 0 {
                return Err(TenancyError::Validation(format!(
                    "no row updated for id {id} — race condition?"
                )));
            }
            // It was active, so no provisioning retry may revive it (#2292).
            let forget = super::provision_store::forget_failed_runs(id)?;
            crate::sql::update_tx(&mut tx, &forget).await?;
            tx.commit().await?;
            super::invalidate_org_cache();
            report.deactivated = true;
        }
        Action::Purge { purge_database } => {
            purge(pools, &registry, &org, purge_database, &mut report).await?;
        }
    }
    Ok(report)
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
    #[cfg(feature = "postgres")]
    if mode == StorageMode::Database && org.backend_kind == "postgres" {
        refuse_registry_database(pools, &pools.resolved_database_url(org).await?)?;
    }
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("Org row has no PK".into()))?;
    // Out of service before anything is destroyed (#1930): if a later
    // step fails, the tenant is inactive and a retry finishes the job.
    Org::objects()
        .where_(Org::id.eq(id))
        .update()
        .set("active", false)
        .execute_pool(registry)
        .await?;
    super::invalidate_org_cache();
    pools.invalidate(slug).await;

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
                {
                    drop_database_at(&url, &super::pools::tenant_session_tag(id)).await?;
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

/// An early, friendly refusal: host, port and database name match the
/// registry pool's. Text-level only; the session check in the drop is the guard.
#[cfg(feature = "postgres")]
fn refuse_registry_database<DB: Database>(
    pools: &TenantPools<DB>,
    tenant_url: &str,
) -> Result<(), TenancyError> {
    use crate::sql::sqlx::postgres::PgConnectOptions;
    use std::str::FromStr;

    let Some(pg) = (pools as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
    else {
        return Ok(());
    };
    // An unparsable URL is refused by `drop_database_at`.
    let Ok(tenant) = PgConnectOptions::from_str(tenant_url) else {
        return Ok(());
    };
    let registry = pg.registry_inner().connect_options();
    if tenant.get_host().eq_ignore_ascii_case(registry.get_host())
        && tenant.get_port() == registry.get_port()
        && tenant.get_database() == registry.get_database()
    {
        return Err(TenancyError::Validation(
            "refusing to drop this tenant's database — it is the registry's own".into(),
        ));
    }
    Ok(())
}

/// `DROP DATABASE` through an admin connection on the same server.
#[cfg(feature = "postgres")]
async fn drop_database_at(tenant_url: &str, tag: &str) -> Result<(), TenancyError> {
    use crate::sql::sqlx::postgres::PgConnectOptions;
    use crate::sql::sqlx::ConnectOptions;
    use std::str::FromStr;

    let opts = PgConnectOptions::from_str(tenant_url).map_err(|e| {
        TenancyError::Validation(format!(
            "cannot parse the tenant's database URL: {e} ({})",
            crate::sql::connect_diagnosis::redact(tenant_url)
        ))
    })?;
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
    let mut admin = opts.clone().database("postgres").connect().await?;
    // Only this tenant's tagged pool sessions, on any pod (#2291). Any other
    // session (registry, another tenant, an operator) still blocks the drop.
    crate::sql::sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND application_name = $2",
    )
    .bind(&dbname)
    .bind(tag)
    .execute(&mut admin)
    .await?;
    // Postgres by construction — the caller checks `backend_kind`.
    let sql = format!(
        "DROP DATABASE IF EXISTS \"{}\"",
        dbname.replace('"', "\"\"")
    );
    match crate::sql::sqlx::query(&sql).execute(&mut admin).await {
        Ok(_) => Ok(()),
        // 55006 object_in_use: a session this tenant's pools did not open.
        Err(crate::sql::sqlx::Error::Database(e)) if e.code().as_deref() == Some("55006") => {
            Err(TenancyError::Validation(format!(
                "database `{dbname}` still has sessions this tenant's pools did not open \
                 (another tenant, the registry or an operator); close them and purge again"
            )))
        }
        Err(e) => Err(e.into()),
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
