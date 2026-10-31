//! What SSO decides outside the sign-in itself: whether a user may use a
//! password, and when an idle SSO-made tenant account is switched off.
//!
//! `sso_only` covers password logins only. API keys and MCP agent secrets
//! are separate credentials and keep working.

use std::collections::HashMap;

use crate::core::SqlValue;
use crate::query::QuerySet;
use crate::sql::Pool;

use super::link::{LinkSource, SsoLink};
use super::policy::Lifecycle;
use super::provider::{load_rows, read_lifecycle, SsoProvider};

/// The rules of a link whose provider row is gone: `sso_only`, until an
/// admin removes the link.
const GONE: Lifecycle = Lifecycle {
    sso_only: true,
    auto_deactivate_after_days: None,
    session_ttl_secs: None,
    superuser_session_ttl_secs: None,
};

/// Whether this tenant's SSO rules let `user` sign in with a password, or
/// reset or set one. Call it only after a right password, so the refusal
/// reads as a wrong one; its reads are not timing-hidden.
///
/// A superuser follows only `password_login`, and when the settings cannot
/// be read still gets in (break-glass); for anyone else `sso_only`
/// (tenant-wide, or on a provider they are linked through, even a disabled
/// or deleted one) refuses. `registry` reads shared providers; without it a
/// user linked through one is refused. Any other unreadable rule refuses.
#[cfg(feature = "tenancy")]
pub async fn password_login_allowed(
    pool: &Pool,
    registry: Option<&Pool>,
    user: &crate::tenancy::User,
) -> bool {
    let settings = match super::policy::TenantSettings::load(pool).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "rustango::sso", "tenant SSO settings unreadable, only superusers use a password: {e}");
            return user.is_superuser;
        }
    };
    if !settings.password_allowed(user.is_superuser) {
        return false;
    }
    if user.is_superuser {
        return true;
    }
    let Some(&user_id) = user.id.get() else {
        return false;
    };
    let sources = [LinkSource::Tenant, LinkSource::Shared];
    match sso_only(pool, registry, user_id, &sources).await {
        Ok(only) => !only,
        Err(e) => {
            tracing::error!(target: "rustango::sso", user_id, "SSO rules unreadable, password refused: {e}");
            false
        }
    }
}

/// [`password_login_allowed`] for a bare-admin user: a non-superuser linked
/// through an `sso_only` provider is refused.
#[cfg(feature = "admin")]
pub(crate) async fn admin_password_allowed(pool: &Pool, user_id: i64, superuser: bool) -> bool {
    if superuser {
        return true;
    }
    match sso_only(pool, None, user_id, &[LinkSource::Admin]).await {
        Ok(only) => !only,
        Err(e) => {
            tracing::error!(target: "rustango::sso", user_id, "SSO rules unreadable, password refused: {e}");
            false
        }
    }
}

/// `(source, provider id)` of each of `user_id`'s links. A missing table holds none.
async fn links_of(pool: &Pool, user_id: i64) -> Result<Vec<(String, i64)>, String> {
    let rows = match QuerySet::<SsoLink>::new()
        .filter("user_id", user_id)
        .only(&["provider_source", "provider_id"])
        .fetch(pool)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return match crate::migrate::try_table_exists_here(pool, "rustango_sso_links").await {
                Ok(false) => Ok(Vec::new()),
                _ => Err(format!("links: {e}")),
            }
        }
    };
    Ok(rows
        .iter()
        .map(|r| (text(r, "provider_source"), int(r, "provider_id")))
        .collect())
}

fn text(r: &HashMap<String, SqlValue>, c: &str) -> String {
    match r.get(c) {
        Some(SqlValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn int(r: &HashMap<String, SqlValue>, c: &str) -> i64 {
    match r.get(c) {
        Some(SqlValue::I64(n)) => *n,
        Some(SqlValue::I32(n)) => i64::from(*n),
        _ => 0,
    }
}

/// The rules of the provider a link points at, enabled or not, with a
/// tenant override of a shared one. A deleted provider is [`GONE`].
/// `None`: the link names no provider (app links).
async fn provider_rules(
    pool: &Pool,
    registry: Option<&Pool>,
    source: &str,
    id: i64,
) -> Result<Option<Lifecycle>, String> {
    if source == LinkSource::Tenant.as_str() || source == LinkSource::Admin.as_str() {
        let rows = load_rows(QuerySet::<SsoProvider>::new().filter("id", id), pool)
            .await
            .map_err(|e| e.to_string())?;
        if rows.is_empty() {
            return Ok(Some(GONE));
        }
        return read_lifecycle::<SsoProvider>(pool, id)
            .await
            .map(Some)
            .map_err(|e| e.0);
    }
    if source == LinkSource::Shared.as_str() {
        return shared_rules(pool, registry, id).await;
    }
    Ok(None)
}

#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
async fn shared_rules(
    pool: &Pool,
    registry: Option<&Pool>,
    id: i64,
) -> Result<Option<Lifecycle>, String> {
    use crate::tenancy::sso::SharedSsoProvider;
    let registry = registry.ok_or("a shared provider needs the registry to read")?;
    let rows = load_rows(
        QuerySet::<SharedSsoProvider>::new().filter("id", id),
        registry,
    )
    .await
    .map_err(|e| e.to_string())?;
    let Some(row) = rows.first() else {
        return Ok(Some(GONE));
    };
    let own = read_lifecycle::<SharedSsoProvider>(registry, id)
        .await
        .map_err(|e| e.0)?;
    let tenant = super::policy::SharedOverride::load(pool, &row.slug)
        .await
        .map_err(|e| e.0)?;
    Ok(Some(match tenant {
        Some(ov) => own.tighten(ov.lifecycle),
        None => own,
    }))
}

#[cfg(not(all(feature = "tenancy", feature = "admin-sso")))]
async fn shared_rules(_: &Pool, _: Option<&Pool>, _: i64) -> Result<Option<Lifecycle>, String> {
    // No shared provider signs anyone in in this build.
    Ok(None)
}

/// Whether `user_id` is linked, through one of `sources`, to an `sso_only` provider.
async fn sso_only(
    pool: &Pool,
    registry: Option<&Pool>,
    user_id: i64,
    sources: &[LinkSource],
) -> Result<bool, String> {
    for (source, id) in links_of(pool, user_id).await? {
        if !sources.iter().any(|s| s.as_str() == source) {
            continue;
        }
        if provider_rules(pool, registry, &source, id)
            .await?
            .is_some_and(|l| l.sso_only)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Switch off tenant users whose every link was made by SSO sign-up and
/// has not been used for its provider's `auto_deactivate_after_days`, and
/// who cannot use a password here ([`password_login_allowed`]): `active`
/// goes false and their sessions end. Never a superuser, nor a user with a
/// link an admin made. The write re-checks that no link was used since.
/// A link through a shared provider is read through `registry`; without it
/// that user is left alone. Returns the user ids.
///
/// # Errors
/// A read or write failed.
#[cfg(feature = "tenancy")]
pub async fn deactivate_idle(pool: &Pool, registry: Option<&Pool>) -> Result<Vec<i64>, String> {
    use crate::sql::FetcherPool as _;
    use crate::tenancy::User;
    let links = match SsoLink::objects().fetch(pool).await {
        Ok(l) => l,
        Err(e) => {
            return match crate::migrate::try_table_exists_here(pool, "rustango_sso_links").await {
                Ok(false) => Ok(Vec::new()),
                _ => Err(format!("links: {e}")),
            }
        }
    };
    let mut by_user: HashMap<i64, Vec<&SsoLink>> = HashMap::new();
    for l in &links {
        by_user.entry(l.user_id).or_default().push(l);
    }
    let now = chrono::Utc::now();
    let mut rules: HashMap<(String, i64), Option<Lifecycle>> = HashMap::new();
    // Each idle user with the earliest cutoff among its links.
    let mut idle = Vec::new();
    'users: for (user_id, links) in by_user {
        let mut earliest = now;
        for l in &links {
            if !l.provisioned {
                continue 'users;
            }
            let key = (l.provider_source.clone(), l.provider_id);
            let rule = match rules.get(&key) {
                Some(r) => *r,
                None => {
                    let r = provider_rules(pool, registry, &key.0, key.1)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!(target: "rustango::sso", user_id, "idle check skipped: {e}");
                            None
                        });
                    rules.insert(key, r);
                    r
                }
            };
            let Some(days) = rule.and_then(|r| r.auto_deactivate_after_days) else {
                continue 'users;
            };
            let cutoff = now - chrono::TimeDelta::days(i64::from(days));
            let last = l.last_used_at.or_else(|| l.created_at.get().copied());
            if last.is_none_or(|at| at >= cutoff) {
                continue 'users;
            }
            earliest = earliest.min(cutoff);
        }
        idle.push((user_id, earliest));
    }
    let mut done = Vec::new();
    for (user_id, cutoff) in idle {
        use crate::sql::UpdaterPool as _;
        let Some(user) = crate::tenancy::member_auth::find_tenant_user(pool, user_id)
            .await
            .map_err(|e| format!("user {user_id}: {e}"))?
        else {
            continue;
        };
        // A user who may still use a password is not SSO-only: leave it.
        if user.is_superuser || password_login_allowed(pool, registry, &user).await {
            continue;
        }
        let used_since = SsoLink::objects()
            .filter("last_used_at__gte", cutoff)
            .values_list_flat("user_id")
            .compile()
            .map_err(|e| e.to_string())?;
        let n = User::objects()
            .filter("id", user_id)
            .filter("is_superuser", false)
            .filter("active", true)
            .where_not_in_subquery("id", used_since)
            .update()
            .set("active", false)
            .set("sessions_revoked_at", now)
            .execute_pool(pool)
            .await
            .map_err(|e| format!("deactivate user {user_id}: {e}"))?;
        if n == 0 {
            continue;
        }
        let entry = crate::audit::PendingEntry {
            entity_table: "rustango_users",
            entity_pk: user_id.to_string(),
            operation: crate::audit::AuditOp::Action,
            source: crate::audit::AuditSource::Custom("sso".into()),
            changes: serde_json::json!({"event": "sso_deactivated_idle", "active": false}),
        };
        if let Err(e) = crate::audit::emit_one_pool(pool, &entry).await {
            tracing::error!(target: "rustango::sso", user_id, error = %e, "an idle deactivation was NOT written to the audit log");
        }
        done.push(user_id);
    }
    Ok(done)
}
