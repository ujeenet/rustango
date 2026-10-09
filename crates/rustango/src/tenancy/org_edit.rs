//! Changing a tenant's routing and display config, from either surface.
//!
//! The console has edited these since v0.25. The CLI could not, so a
//! host-pattern change or a credential rotation meant a browser (#1344).
//!
//! ## Why this is not just an UPDATE
//!
//! Three things have to happen together, and the order matters:
//!
//! 1. Write the row.
//! 2. Drop the cached `Org`. Resolution serves from that cache, so
//!    without this the pool is evicted and immediately rebuilt from the
//!    stale row — a rotation reports success while the next request
//!    reconnects with the old credential, and `active = false` keeps
//!    serving.
//! 3. Evict the tenant's pool, but only when the URL actually changed —
//!    evicting on every edit throws away warm connections for a display
//!    name.
//!
//! Step 2 is the one that is easy to leave out and impossible to notice
//! in testing, because the stale read only shows up on another request.
//! So both surfaces call [`apply`] rather than each remembering.

use crate::core::{Column as _, Model as _};
use crate::sql::{FetcherPool as _, Pool};
use crate::tenancy::error::TenancyError;
use crate::tenancy::Org;

/// Which fields an edit touches. `None` means "leave alone" — distinct
/// from `Some("")`, which clears an optional column.
#[derive(Debug, Default, Clone)]
pub struct OrgPatch {
    pub display_name: Option<String>,
    pub host_pattern: Option<String>,
    pub path_prefix: Option<String>,
    pub port: Option<String>,
    pub active: Option<bool>,
    pub database_url: Option<String>,
}

impl OrgPatch {
    /// Nothing to do — the caller should say so rather than run an UPDATE
    /// with an empty SET, which is a SQL error on some backends and a
    /// silent no-op on others.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.display_name.is_none()
            && self.host_pattern.is_none()
            && self.path_prefix.is_none()
            && self.port.is_none()
            && self.active.is_none()
            && self.database_url.is_none()
    }

    /// Column names this patch touches, for the audit trail.
    ///
    /// `database_url` is named but its value never is: the fact of a
    /// rotation belongs in the log, the credential does not.
    #[must_use]
    pub fn touched(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.display_name.is_some() {
            out.push("display_name");
        }
        if self.host_pattern.is_some() {
            out.push("host_pattern");
        }
        if self.path_prefix.is_some() {
            out.push("path_prefix");
        }
        if self.port.is_some() {
            out.push("port");
        }
        if self.active.is_some() {
            out.push("active");
        }
        if self.database_url.is_some() {
            out.push("database_url");
        }
        out
    }
}

/// What [`apply`] changed, so the caller can word its own message.
#[derive(Debug)]
pub struct Applied {
    pub touched: Vec<&'static str>,
    /// True only when the URL is genuinely different from the stored one.
    pub database_url_rotated: bool,
}

/// Validate, write, and drop the cached `Org` — in that order.
///
/// Evicting the tenant's *pool* is the caller's, because `TenantPools` is
/// generic over the backend and this module is not: do it when
/// [`Applied::database_url_rotated`] is true, and not otherwise.
///
/// # Errors
/// [`TenancyError::Validation`] for a bad host pattern, port, or path
/// prefix, or when the tenant does not exist; driver errors otherwise.
pub async fn apply(registry: &Pool, slug: &str, patch: &OrgPatch) -> Result<Applied, TenancyError> {
    use crate::core::SqlValue;
    if patch.is_empty() {
        return Err(TenancyError::Validation(
            "nothing to change — pass at least one field".into(),
        ));
    }
    let mut values: Vec<(&'static str, SqlValue)> = Vec::new();
    if let Some(v) = patch.display_name.as_deref() {
        // NOT NULL, unlike the routing columns — an empty display name is
        // the empty string; NULL would be a constraint violation.
        values.push(("display_name", SqlValue::String(v.trim().to_owned())));
    }
    if let Some(v) = patch.host_pattern.as_deref() {
        values.push(("host_pattern", SqlValue::String(v.to_owned())));
    }
    if let Some(v) = patch.path_prefix.as_deref() {
        values.push(("path_prefix", SqlValue::String(v.to_owned())));
    }
    if let Some(v) = patch.port.as_deref() {
        let p = if v.trim().is_empty() {
            SqlValue::Null
        } else {
            SqlValue::I32(
                v.trim()
                    .parse::<i32>()
                    .map_err(|_| TenancyError::Validation(format!("port `{v}` is not a number")))?,
            )
        };
        values.push(("port", p));
    }
    if let Some(v) = patch.active {
        values.push(("active", SqlValue::Bool(v)));
    }
    if let Some(v) = patch.database_url.as_deref() {
        values.push(("database_url", SqlValue::String(v.to_owned())));
    }
    let mut applied = apply_values(registry, slug, values).await?;
    applied.touched = patch.touched();
    Ok(applied)
}

/// The one write path for an `Org` edit: the console's form values and
/// [`apply`]'s patch both pass the routing validators and the host-clash
/// check here (#1931).
pub(crate) async fn apply_values(
    registry: &Pool,
    slug: &str,
    values: Vec<(&'static str, crate::core::SqlValue)>,
) -> Result<Applied, TenancyError> {
    use crate::core::SqlValue;
    let existing = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(registry)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))?;
    let existing_id = existing.id.get().copied().unwrap_or_default();

    // The same validators provisioning uses, so a value the console or
    // the CLI can set is a value `create-tenant` would have accepted.
    let blank = |v: &SqlValue| match v {
        SqlValue::Null => true,
        SqlValue::String(s) => s.trim().is_empty(),
        _ => false,
    };
    let mut set: Vec<crate::core::Assignment> = Vec::new();
    let mut touched: Vec<&'static str> = Vec::new();
    let mut database_url_rotated = false;
    let mut claim: Option<String> = None;
    // An operator activated it, so no provisioning retry may resume it (#2292).
    let activating = values
        .iter()
        .any(|(c, v)| *c == "active" && matches!(v, SqlValue::Bool(true)));
    for (column, value) in values {
        let value = match (column, value) {
            // Blank keeps the current URL; it is a credential, never cleared here.
            ("database_url", v) if blank(&v) => continue,
            ("database_url", SqlValue::String(new)) => {
                crate::tenancy::provision::refuse_registry_pool(&new, registry)
                    .map_err(TenancyError::Validation)?;
                database_url_rotated = existing.database_url.as_deref() != Some(new.as_str());
                SqlValue::String(new)
            }
            ("host_pattern" | "path_prefix", v) if blank(&v) => SqlValue::Null,
            ("host_pattern", SqlValue::String(v)) => {
                // Store the normalized form so it matches a `Host` header byte-for-byte.
                let host = crate::tenancy::provision::validate_host_pattern(&v)
                    .map_err(TenancyError::Validation)?;
                // Claimed in the UPDATE's transaction below (#2099).
                claim = Some(host.clone());
                SqlValue::String(host)
            }
            ("path_prefix", SqlValue::String(v)) => {
                let v = v.trim().to_owned();
                crate::tenancy::provision::validate_path_prefix(&v)
                    .map_err(TenancyError::Validation)?;
                if crate::tenancy::org_host::prefix_claimed(registry, &v, Some(existing_id)).await?
                {
                    return Err(TenancyError::Validation(format!(
                        "path prefix `{v}` is already used by another tenant"
                    )));
                }
                SqlValue::String(v)
            }
            ("port", SqlValue::I32(n)) => {
                crate::tenancy::provision::validate_port(n).map_err(TenancyError::Validation)?;
                if crate::tenancy::org_host::port_claimed(registry, n, Some(existing_id)).await? {
                    return Err(TenancyError::Validation(format!(
                        "port {n} is already used by another tenant"
                    )));
                }
                SqlValue::I32(n)
            }
            (_, v) => v,
        };
        touched.push(column);
        set.push(assign(column, value));
    }

    if set.is_empty() {
        return Err(TenancyError::Validation(
            "nothing to change — pass at least one field".into(),
        ));
    }

    let update = crate::core::UpdateQuery {
        model: Org::SCHEMA,
        set,
        where_clause: crate::core::WhereExpr::and_predicates(vec![crate::core::Filter {
            column: "slug",
            op: crate::core::Op::Eq,
            value: crate::core::SqlValue::String(slug.to_owned()),
        }]),
    };
    use crate::tenancy::org_host::{claim_error, claim_host, Claimant};
    let mut tx = match &claim {
        Some(host) => {
            claim_host(registry, host, Claimant::Base(existing_id))
                .await
                .map_err(claim_error)?
                .tx
        }
        None => crate::sql::write_transaction_pool(registry).await?,
    };
    crate::sql::update_tx(&mut tx, &update).await?;
    if existing.active || activating {
        let forget = crate::tenancy::provision_store::forget_unsucceeded_runs(existing_id)?;
        crate::sql::update_tx(&mut tx, &forget).await?;
    }
    tx.commit().await?;

    // Before anything else acts on the write — see the module docs.
    crate::tenancy::invalidate_org_cache();

    Ok(Applied {
        touched,
        database_url_rotated,
    })
}

fn assign(column: &'static str, value: crate::core::SqlValue) -> crate::core::Assignment {
    crate::core::Assignment {
        column,
        value: value.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_patch_is_refused_rather_than_run() {
        assert!(OrgPatch::default().is_empty());
        let p = OrgPatch {
            display_name: Some("Acme".into()),
            ..OrgPatch::default()
        };
        assert!(!p.is_empty());
    }

    /// The audit trail names the column, and the blank-clears-it case is
    /// still a change worth recording.
    #[test]
    fn touched_lists_every_supplied_field() {
        let p = OrgPatch {
            display_name: Some(String::new()),
            host_pattern: Some("shop.test".into()),
            active: Some(false),
            ..OrgPatch::default()
        };
        assert_eq!(p.touched(), vec!["display_name", "host_pattern", "active"]);
    }
}
