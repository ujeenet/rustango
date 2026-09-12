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
    if patch.is_empty() {
        return Err(TenancyError::Validation(
            "nothing to change — pass at least one field".into(),
        ));
    }

    let existing = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(registry)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))?;

    // The same validators provisioning uses, so a value the console or
    // the CLI can set is a value `create-tenant` would have accepted.
    let mut set: Vec<crate::core::Assignment> = Vec::new();
    if let Some(v) = patch.display_name.as_deref() {
        // NOT NULL, unlike the routing columns below — an empty display
        // name is the empty string, and writing NULL would be a constraint
        // violation rather than a clear.
        set.push(assign(
            "display_name",
            crate::core::SqlValue::String(v.trim().to_owned()),
        ));
    }
    if let Some(v) = patch.host_pattern.as_deref() {
        // Returns the normalized (lowercased) form — store that, not what
        // was typed, so it can match a `Host` header byte-for-byte.
        let normalized = if v.trim().is_empty() {
            None
        } else {
            Some(
                crate::tenancy::provision::validate_host_pattern(v)
                    .map_err(TenancyError::Validation)?,
            )
        };
        set.push(assign("host_pattern", string_or_null(normalized)));
    }
    if let Some(v) = patch.path_prefix.as_deref() {
        let cleaned = if v.trim().is_empty() {
            None
        } else {
            crate::tenancy::provision::validate_path_prefix(v).map_err(TenancyError::Validation)?;
            Some(v.trim().to_owned())
        };
        set.push(assign("path_prefix", string_or_null(cleaned)));
    }
    if let Some(v) = patch.port.as_deref() {
        let p = if v.trim().is_empty() {
            None
        } else {
            let n = v
                .trim()
                .parse::<i32>()
                .map_err(|_| TenancyError::Validation(format!("port `{v}` is not a number")))?;
            crate::tenancy::provision::validate_port(n).map_err(TenancyError::Validation)?;
            Some(n)
        };
        set.push(assign(
            "port",
            p.map_or(crate::core::SqlValue::Null, crate::core::SqlValue::I32),
        ));
    }
    if let Some(v) = patch.active {
        set.push(assign("active", crate::core::SqlValue::Bool(v)));
    }
    let database_url_rotated = patch
        .database_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .is_some_and(|new| existing.database_url.as_deref() != Some(new));
    if let Some(v) = patch
        .database_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
    {
        set.push(assign(
            "database_url",
            crate::core::SqlValue::String(v.to_owned()),
        ));
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
    crate::sql::update_pool(registry, &update).await?;

    // Before anything else acts on the write — see the module docs.
    crate::tenancy::invalidate_org_cache();

    Ok(Applied {
        touched: patch.touched(),
        database_url_rotated,
    })
}

fn assign(column: &'static str, value: crate::core::SqlValue) -> crate::core::Assignment {
    crate::core::Assignment {
        column,
        value: value.into(),
    }
}

/// The nullable routing columns: absent means NULL, not "".
fn string_or_null(v: Option<String>) -> crate::core::SqlValue {
    v.map_or(crate::core::SqlValue::Null, crate::core::SqlValue::String)
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

    /// The routing columns are nullable, so clearing one means NULL — an
    /// empty string there would be a value the resolver compares against.
    #[test]
    fn a_cleared_routing_column_becomes_null() {
        assert!(matches!(string_or_null(None), crate::core::SqlValue::Null));
        assert!(matches!(
            string_or_null(Some("shop.test".into())),
            crate::core::SqlValue::String(_)
        ));
    }
}
