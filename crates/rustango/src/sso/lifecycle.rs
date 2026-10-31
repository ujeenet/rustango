//! What SSO decides outside the sign-in itself: whether a tenant user may
//! use a password, and when an idle SSO-made account is switched off.

use std::collections::HashMap;

use crate::core::SqlValue;
use crate::query::QuerySet;
use crate::sql::{FetcherPool as _, Pool};
use crate::tenancy::User;

use super::link::{LinkSource, SsoLink};
use super::policy::{Lifecycle, TenantSettings};
use super::provider::{load_rows, read_lifecycle, SsoProvider};

/// Whether this tenant's SSO rules let `user` sign in with a password, or
/// reset or set one. Call it after the password check, so a refusal takes
/// the same time. A superuser follows only `password_login`; for anyone else
/// `sso_only` (tenant-wide, or on a provider they are linked through) refuses.
/// `registry` reads shared providers; without it a user linked through one
/// is refused. An unreadable rule refuses.
pub async fn password_login_allowed(pool: &Pool, registry: Option<&Pool>, user: &User) -> bool {
    let settings = match TenantSettings::load(pool).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "rustango::sso", "tenant SSO settings unreadable, password refused: {e}");
            return false;
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
    match sso_only(pool, registry, user_id).await {
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

/// The rules of the enabled provider a link points at, with an override
/// of a shared one. `None`: the provider is gone or disabled.
async fn provider_rules(
    pool: &Pool,
    registry: Option<&Pool>,
    source: &str,
    id: i64,
) -> Result<Option<Lifecycle>, String> {
    let err = |e: &dyn std::fmt::Display| e.to_string();
    if source == LinkSource::Tenant.as_str() {
        let rows = load_rows(QuerySet::<SsoProvider>::new().filter("id", id), pool)
            .await
            .map_err(|e| err(&e))?;
        if !rows.iter().any(|r| r.enabled) {
            return Ok(None);
        }
        return read_lifecycle::<SsoProvider>(pool, id)
            .await
            .map(Some)
            .map_err(|e| err(&e.0));
    }
    if source == LinkSource::Shared.as_str() {
        return shared_rules(pool, registry, id).await;
    }
    // Admin and app links name no tenant provider.
    Ok(None)
}

#[cfg(feature = "admin-sso")]
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
    let Some(row) = rows.iter().find(|r| r.enabled) else {
        return Ok(None);
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

#[cfg(not(feature = "admin-sso"))]
async fn shared_rules(_: &Pool, _: Option<&Pool>, _: i64) -> Result<Option<Lifecycle>, String> {
    // No shared provider signs anyone in without `admin-sso`.
    Ok(None)
}

/// Whether `user_id` is linked through an enabled `sso_only` provider.
async fn sso_only(pool: &Pool, registry: Option<&Pool>, user_id: i64) -> Result<bool, String> {
    for (source, id) in links_of(pool, user_id).await? {
        if provider_rules(pool, registry, &source, id)
            .await?
            .is_some_and(|l| l.sso_only)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Switch off users whose every link was made by SSO sign-up and has not
/// been used for its provider's `auto_deactivate_after_days`: `active` goes
/// false and their sessions end. Never a superuser, nor a user with a link
/// an admin made. A link through a shared provider is read through
/// `registry`; without it that user is left alone. Returns the user ids.
///
/// # Errors
/// A read or write failed.
pub async fn deactivate_idle(pool: &Pool, registry: Option<&Pool>) -> Result<Vec<i64>, String> {
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
    let mut idle = Vec::new();
    'users: for (user_id, links) in by_user {
        for l in &links {
            if !l.provisioned {
                continue 'users;
            }
            let key = (l.provider_source.clone(), l.provider_id);
            let rule = match rules.get(&key) {
                Some(r) => *r,
                None => {
                    let r = match provider_rules(pool, registry, &key.0, key.1).await {
                        Ok(r) => r,
                        // Unreadable: leave the user alone.
                        Err(e) => {
                            tracing::warn!(target: "rustango::sso", user_id, "idle check skipped: {e}");
                            None
                        }
                    };
                    rules.insert(key, r);
                    r
                }
            };
            let Some(days) = rule.and_then(|r| r.auto_deactivate_after_days) else {
                continue 'users;
            };
            let last = l.last_used_at.or_else(|| l.created_at.get().copied());
            let cutoff = now - chrono::TimeDelta::days(i64::from(days));
            if last.is_none_or(|at| at >= cutoff) {
                continue 'users;
            }
        }
        idle.push(user_id);
    }
    let mut done = Vec::new();
    for user_id in idle {
        use crate::sql::UpdaterPool as _;
        let n = User::objects()
            .filter("id", user_id)
            .filter("is_superuser", false)
            .filter("active", true)
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
