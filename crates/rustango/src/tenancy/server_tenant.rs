//! A tenant whose database rustango made on a saved server, known by its
//! [`TenantDatabase`] record. Only such a database is dropped by a purge
//! or has its login's password rotated, and only while the tenant's URL
//! still names it.
//!
//! ## Rotating the password
//!
//! Alter the login, then seal and store the new URL over the old one
//! (compare-and-set), then drop this pod's pools. Other pods pick the new
//! URL up on their next `rustango_orgs` fingerprint poll; until then their
//! new connections use the old password. On MySQL 8.0.14+ the old password
//! is retained over that window and discarded after it. Open connections
//! are never cut.

use std::time::Duration;

use sqlx::Database;

use super::db_server::{self, DbServer, DbServerError, TenantDatabase};
use super::error::TenancyError;
use super::operation::{OpReporter, OperationObserver, OperationStep as Step};
use super::org::{Org, StorageMode};
use super::pools::TenantPools;
use super::provision_store::{self as store, RunKind, RunText};
use super::server_admin::ServerAdmin;
use crate::core::Column as _;
use crate::sql::{DatabaseName, Login, LoginName, LoginPassword, UpdaterPool as _};

/// How long a saved server's admin login gets to connect.
const ADMIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A tenant database rustango made, still named by the tenant's URL.
/// Built only by [`ServerTenant::of`], which checks that.
pub(crate) struct ServerTenant {
    pub(crate) record: TenantDatabase,
    pub(crate) server: DbServer,
    pub(crate) database: DatabaseName,
    /// `None` on SQLite.
    pub(crate) login: Option<Login>,
}

impl ServerTenant {
    /// The record of `org`'s database, checked against its resolved `url`.
    /// `Ok(None)` when rustango did not make it. A record the URL no
    /// longer names (after `edit-tenant` or `migrate-tenant-storage`) is
    /// refused: dropping it, or keeping it, could hit the wrong database.
    pub(crate) async fn of(
        registry: &crate::sql::Pool,
        org: &Org,
        url: &str,
    ) -> Result<Option<Self>, TenancyError> {
        let Some(record) = TenantDatabase::of_org(registry, org).await? else {
            return Ok(None);
        };
        let server = db_server::get(registry, &record.server_name)
            .await
            .map_err(|e| match e {
                DbServerError::NotFound(name) => TenancyError::Validation(format!(
                    "tenant `{}`'s database was made on database server `{name}`, which is no \
                     longer saved; save it again to reach that database",
                    org.slug
                )),
                other => other.into(),
            })?;
        if let Err(why) = server.names_record(url, &record) {
            return Err(TenancyError::Validation(format!(
                "tenant `{}`'s database URL no longer names the database rustango made \
                 (`{}` on server `{}`): {why}. It was edited or moved, so nothing is dropped \
                 or changed. Put the URL back, or drop that database by hand and delete its \
                 rustango_tenant_databases row",
                org.slug, record.database_name, server.name
            )));
        }
        let bad = |e: crate::sql::InvalidName| TenancyError::Validation(e.to_string());
        let database = DatabaseName::new(record.database_name.clone()).map_err(bad)?;
        let login = record
            .login_name
            .clone()
            .map(LoginName::new)
            .transpose()
            .map_err(bad)?
            .map(Login::new);
        Ok(Some(Self {
            record,
            server,
            database,
            login,
        }))
    }

    pub(crate) async fn connect(&self) -> Result<ServerAdmin, TenancyError> {
        Ok(self.server.connect(ADMIN_TIMEOUT).await?)
    }
}

/// Why a PG 16+ admin may not touch a login another admin made.
pub(crate) fn admin_option_refusal(login: &LoginName, server: &str) -> TenancyError {
    TenancyError::Validation(format!(
        "the admin login of server `{server}` may not alter or drop login `{login}`: on \
         PostgreSQL 16+ that needs ADMIN OPTION on the role, which only the admin that \
         created it holds. As a superuser or that admin, run \
         `GRANT \"{login}\" TO <admin> WITH ADMIN OPTION`, then try again"
    ))
}

/// Rotate the password of `slug`'s login. See the module docs.
///
/// # Errors
/// A refusal (no record, a URL that no longer matches, an external secret
/// reference, missing rights) before anything changes, or a failed step.
pub async fn rotate_tenant_password<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    observer: Option<&dyn OperationObserver>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let org = tenant(pools, slug).await?;
    let rep = OpReporter::new(observer);
    rotate_reported(pools, &org, &rep).await
}

/// [`rotate_tenant_password`], recorded as a run of kind
/// [`RunKind::RotatePassword`].
///
/// # Errors
/// As [`rotate_tenant_password`]; the run is closed either way.
pub async fn rotate_password_recorded<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    observer: Option<&dyn OperationObserver>,
    requested_by: Option<&str>,
) -> Result<store::ProvisioningRun, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let org = tenant(pools, slug).await?;
    let run =
        store::open_operation_run(&registry, RunKind::RotatePassword, &org, requested_by).await?;
    let run_id = run.id.get().copied().unwrap_or_default();
    let result = rotate_in(pools, &org, observer, run_id).await;
    let run = store::run_by_id(&registry, run_id).await?.unwrap_or(run);
    result.map(|()| run)
}

/// [`rotate_tenant_password`] into a run the caller opened.
///
/// # Errors
/// As [`rotate_tenant_password`]; the run is closed either way.
pub async fn rotate_password_in_run<DB: Database>(
    pools: &TenantPools<DB>,
    slug: &str,
    observer: Option<&dyn OperationObserver>,
    run_id: i64,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let org = match tenant(pools, slug).await {
        Ok(o) => o,
        Err(e) => {
            let rep = OpReporter::new(observer).persisting(&registry, run_id, slug);
            let failed = rep.fail::<()>(Step::Check, e).await;
            rep.finish(failed.as_ref().err()).await;
            return failed;
        }
    };
    rotate_in(pools, &org, observer, run_id).await
}

async fn rotate_in<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    observer: Option<&dyn OperationObserver>,
    run_id: i64,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    let rep = OpReporter::new(observer).persisting(&registry, run_id, &org.slug);
    let result = rotate_reported(pools, org, &rep).await;
    rep.finish(result.as_ref().err()).await;
    result
}

async fn tenant<DB: Database>(pools: &TenantPools<DB>, slug: &str) -> Result<Org, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .first(&pools.registry_pool())
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("no tenant with slug `{slug}`")))
}

/// Everything a rotation needs, checked before anything changes.
struct Rotation {
    tenant: ServerTenant,
    login: Login,
    admin: ServerAdmin,
    /// The resolved URL, old password inside.
    url: String,
    /// `Org.database_url` as read: the compare-and-set's expected value.
    stored: String,
    old: Option<LoginPassword>,
    retain: bool,
}

async fn check_rotation<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
) -> Result<Rotation, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let slug = &org.slug;
    if org.storage_mode != StorageMode::Database.as_str() {
        return Err(TenancyError::Validation(format!(
            "tenant `{slug}` is not database-mode, so it has no login of its own"
        )));
    }
    let stored = org
        .database_url
        .clone()
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` has no database URL")))?;
    let url = pools.resolved_database_url(org).await?;
    let registry = pools.registry_pool();
    let tenant = ServerTenant::of(&registry, org, &url)
        .await?
        .ok_or_else(|| {
            TenancyError::Validation(format!(
                "tenant `{slug}`'s database was not made by rustango on a saved server, so its \
             login is not rustango's to change; change the password where it was made"
            ))
        })?;
    let login = tenant.login.clone().ok_or_else(|| {
        TenancyError::Validation(format!(
            "tenant `{slug}` is on SQLite, which has no password"
        ))
    })?;
    // Only a literal or sealed URL can be written back; a reference names
    // a secret store this registry cannot change.
    if !(stored == url || stored.starts_with("enc:")) {
        return Err(TenancyError::Validation(format!(
            "tenant `{slug}`'s URL is a reference ({}); rotate the password in that secret store",
            super::secrets::shown_reference(&stored)
        )));
    }
    let old = old_password(&url);
    let admin = tenant.connect().await?;
    let checked = async {
        let report = admin.probe().await?;
        if !report.can_create_user {
            return Err(TenancyError::Validation(format!(
                "the admin login of server `{}` may not change logins",
                tenant.server.name
            )));
        }
        let rights = admin.tenant_rights(&tenant.database, Some(&login)).await?;
        if !rights.alter_login {
            return Err(admin_option_refusal(login.name(), &tenant.server.name));
        }
        Ok(report.dual_passwords())
    }
    .await;
    match checked {
        Ok(retain) => Ok(Rotation {
            tenant,
            login,
            admin,
            url,
            stored,
            old,
            retain,
        }),
        Err(e) => {
            admin.close().await;
            Err(e)
        }
    }
}

async fn rotate_reported<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    rep: &OpReporter<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    rep.started(Step::Check).await;
    let r = match check_rotation(pools, org).await {
        Ok(r) => r,
        Err(e) => return rep.fail(Step::Check, e).await,
    };
    rep.ok(Step::Check).await;
    let result = rotate_checked(pools, org, &r, rep).await;
    r.admin.close().await;
    result
}

async fn rotate_checked<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    r: &Rotation,
    rep: &OpReporter<'_>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let named = |what| RunText::about_database(what, &r.tenant.database, Some(r.login.name()));
    let new = LoginPassword::generate();
    let new_url = match with_password(&r.url, &new) {
        Ok(u) => u,
        Err(e) => return rep.fail(Step::Check, e).await,
    };

    rep.started(Step::AlterPassword).await;
    if let Err(e) = r.admin.alter_password(&r.login, &new, r.retain).await {
        return rep.fail(Step::AlterPassword, e.into()).await;
    }
    rep.done(Step::AlterPassword, named("new password set"))
        .await;

    rep.started(Step::StorePassword).await;
    if let Err(e) = store_url(pools, org, &r.stored, &new_url).await {
        revert(&pools.registry_pool(), org, r, rep).await;
        return rep.fail(Step::StorePassword, e).await;
    }
    super::invalidate_org_cache();
    rep.ok(Step::StorePassword).await;

    rep.started(Step::InvalidatePools).await;
    pools.invalidate(&org.slug).await;
    rep.ok(Step::InvalidatePools).await;

    if !r.retain {
        rep.skipped(
            Step::DiscardOldPassword,
            "this server keeps no old password; it stopped working at once",
        )
        .await;
        return Ok(());
    }
    rep.started(Step::DiscardOldPassword).await;
    // Until every pod has the new URL, the retained password keeps it working.
    tokio::time::sleep(super::resolver::org_change_settles_within()).await;
    if let Err(e) = r.admin.discard_old_password(&r.login).await {
        tracing::warn!(target: "rustango::tenancy::operation", error = %e, "could not discard the old tenant password");
        return rep
            .fail(
                Step::DiscardOldPassword,
                TenancyError::Validation(format!(
                    "the new password is stored, but the old one could not be discarded and still \
                     works; run `ALTER USER '{}'@'%' DISCARD OLD PASSWORD` as the admin",
                    r.login.name()
                )),
            )
            .await;
    }
    rep.done(Step::DiscardOldPassword, named("old password discarded"))
        .await;
    Ok(())
}

/// Write `new` over `Org.database_url` only while it still holds `expected`.
async fn store_url<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    expected: &str,
    new_url: &str,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let stored = super::secrets::stored_form(new_url, new_url, pools.secrets())
        .await
        .map_err(TenancyError::Validation)?;
    let id = org
        .id
        .get()
        .copied()
        .ok_or_else(|| TenancyError::Validation("Org row has no PK".into()))?;
    let n = Org::objects()
        .where_(Org::id.eq(id))
        .where_(Org::database_url.eq(Some(expected.to_owned())))
        .update()
        .set("database_url", Some(stored))
        .execute_pool(&pools.registry_pool())
        .await?;
    if n == 0 {
        return Err(TenancyError::Validation(format!(
            "tenant `{}`'s database URL changed meanwhile, so the new password was not stored",
            org.slug
        )));
    }
    Ok(())
}

/// Put the old password back after a failed store, but only while the
/// stored URL still holds it: a URL changed meanwhile is not ours to undo.
async fn revert(registry: &crate::sql::Pool, org: &Org, r: &Rotation, rep: &OpReporter<'_>) {
    let again = format!(
        "run `rotate-tenant-password {}` again so they agree",
        org.slug
    );
    let failed = |why: String| async move {
        let _ = rep
            .fail::<()>(Step::Revert, TenancyError::Validation(why))
            .await;
    };
    rep.started(Step::Revert).await;
    let still_old = match Org::objects()
        .where_(Org::id.eq(org.id.get().copied().unwrap_or_default()))
        .first(registry)
        .await
    {
        Ok(Some(now)) => now.database_url.as_deref() == Some(r.stored.as_str()),
        _ => false,
    };
    if !still_old {
        return failed(format!(
            "the stored URL changed meanwhile or could not be read, so the old password was not \
             put back; the login has the new password and the URL does not: {again}"
        ))
        .await;
    }
    let Some(old) = &r.old else {
        return failed(format!(
            "the stored URL has no password to put back; the login has the new password and the \
             URL does not: {again}"
        ))
        .await;
    };
    match r.admin.alter_password(&r.login, old, false).await {
        Ok(()) => {
            rep.done(
                Step::Revert,
                RunText::about_database(
                    "old password restored",
                    &r.tenant.database,
                    Some(r.login.name()),
                ),
            )
            .await;
        }
        Err(e) => {
            tracing::error!(target: "rustango::tenancy::operation", error = %e, "could not restore the old tenant password");
            failed(format!(
                "the old password could not be put back; the stored URL holds the old password \
                 and the login the new one: {again}"
            ))
            .await;
        }
    }
}

/// The password in `url`'s userinfo.
fn old_password(url: &str) -> Option<LoginPassword> {
    let (_, userinfo, _) = crate::sql::connect_diagnosis::split_userinfo_exact(url).ok()??;
    let (_, pw) = userinfo.split_once(':')?;
    LoginPassword::new(crate::url_codec::percent_decode_path(pw)).ok()
}

/// `url` with its password replaced; user, host, database and TLS kept.
fn with_password(url: &str, password: &LoginPassword) -> Result<String, TenancyError> {
    let none = || TenancyError::Validation("the tenant URL has no login to give a password".into());
    let (scheme, userinfo, rest) = crate::sql::connect_diagnosis::split_userinfo_exact(url)
        .map_err(TenancyError::Validation)?
        .ok_or_else(none)?;
    let user = userinfo
        .split(':')
        .next()
        .filter(|u| !u.is_empty())
        .ok_or_else(none)?;
    Ok(format!(
        "{scheme}://{user}:{}@{rest}",
        crate::url_codec::url_encode(password.expose())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_swapped_and_nothing_else() {
        let pw = LoginPassword::new("n3w/p@ss").unwrap();
        assert_eq!(
            with_password("postgres://acme:old%2Fpw@db:5432/acme?sslmode=require", &pw).unwrap(),
            "postgres://acme:n3w%2Fp%40ss@db:5432/acme?sslmode=require"
        );
        assert_eq!(
            old_password("mysql://acme:old%2Fpw@db/acme")
                .unwrap()
                .expose(),
            "old/pw"
        );
        assert!(with_password("sqlite:///srv/t/acme.db", &pw).is_err());
        assert!(old_password("postgres://acme@db/acme").is_none());
    }

    #[test]
    fn the_admin_option_refusal_says_what_to_grant() {
        let msg = admin_option_refusal(&LoginName::new("acme").unwrap(), "main").to_string();
        assert!(
            msg.contains("ADMIN OPTION") && msg.contains("GRANT \"acme\""),
            "{msg}"
        );
    }
}
