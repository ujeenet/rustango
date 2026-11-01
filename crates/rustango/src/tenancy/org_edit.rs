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
use crate::tenancy::org_host::{claim_routing, ClaimError, ClaimKey, Claimant};
use crate::tenancy::{Org, SecretsResolver as _, TenantSecrets};

/// Which fields an edit touches. `None` means "leave alone" — distinct
/// from `Some("")`, which clears an optional column.
#[derive(Default, Clone)]
pub struct OrgPatch {
    pub display_name: Option<String>,
    pub host_pattern: Option<String>,
    pub path_prefix: Option<String>,
    pub port: Option<String>,
    pub active: Option<bool>,
    pub database_url: Option<String>,
}

/// The password in `database_url` is masked.
impl std::fmt::Debug for OrgPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrgPatch")
            .field("display_name", &self.display_name)
            .field("host_pattern", &self.host_pattern)
            .field("path_prefix", &self.path_prefix)
            .field("port", &self.port)
            .field("active", &self.active)
            .field(
                "database_url",
                &super::org::redacted_url(self.database_url.as_deref()),
            )
            .finish()
    }
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
/// A new `database_url` is opened with
/// [`ChainSecretsResolver::standard`](crate::tenancy::ChainSecretsResolver::standard);
/// see [`apply_with_secrets`] for a custom resolver.
///
/// # Errors
/// [`TenancyError::Validation`] for a bad host pattern, port, path prefix
/// or database URL, or when the tenant does not exist; driver errors otherwise.
pub async fn apply(registry: &Pool, slug: &str, patch: &OrgPatch) -> Result<Applied, TenancyError> {
    apply_with_secrets(registry, &TenantSecrets::standard(), slug, patch).await
}

/// [`apply`], opening and storing a new `database_url` per `secrets`
/// (the tenant pools') so it is checked like a literal.
///
/// # Errors
/// As [`apply`].
pub async fn apply_with_secrets(
    registry: &Pool,
    secrets: &TenantSecrets,
    slug: &str,
    patch: &OrgPatch,
) -> Result<Applied, TenancyError> {
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
    let mut applied = apply_values(registry, secrets, slug, values).await?;
    applied.touched = patch.touched();
    Ok(applied)
}

/// The one write path for an `Org` edit: the console's form values and
/// [`apply`]'s patch both pass the routing validators and the host-clash
/// check here (#1931).
pub(crate) async fn apply_values(
    registry: &Pool,
    secrets: &TenantSecrets,
    slug: &str,
    values: Vec<(&'static str, crate::core::SqlValue)>,
) -> Result<Applied, TenancyError> {
    apply_values_if(registry, secrets, slug, values, None)
        .await?
        .ok_or_else(|| TenancyError::Validation(format!("tenant `{slug}` not found")))
}

/// [`apply_values`], written only while `database_url` still holds
/// `expected` when that is `Some`; `Ok(None)` when it changed meanwhile.
pub(crate) async fn apply_values_if(
    registry: &Pool,
    secrets: &TenantSecrets,
    slug: &str,
    values: Vec<(&'static str, crate::core::SqlValue)>,
    expected: Option<&str>,
) -> Result<Option<Applied>, TenancyError> {
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
    let mut routing: Vec<(&str, SqlValue)> = Vec::new();
    let mut database_url_rotated = false;
    // Claimed in the UPDATE's transaction below.
    let mut claims: Vec<ClaimKey> = Vec::new();
    // An operator activated it, so no provisioning retry may resume it (#2292).
    let activating = values
        .iter()
        .any(|(c, v)| *c == "active" && matches!(v, SqlValue::Bool(true)));
    // A failed create left the schema half-migrated: only a resume may go live.
    if activating && !existing.active {
        if let Some(run) =
            crate::tenancy::provision_store::failed_creator_run(registry, existing_id).await?
        {
            let run_id = run.id.get().copied().unwrap_or_default();
            return Err(TenancyError::Validation(format!(
                "tenant `{slug}` was left inactive by failed provision run #{run_id}; \
                 resume that run (console run page, or `create-tenant {slug} --resume`) \
                 instead of activating it"
            )));
        }
    }
    for (column, value) in values {
        let value = match (column, value) {
            // Blank keeps the current URL; it is a credential, never cleared here.
            ("database_url", v) if blank(&v) => continue,
            // Nothing reads it in schema mode; moving storage is migrate-tenant-storage.
            ("database_url", _)
                if matches!(
                    super::org::StorageMode::parse(&existing.storage_mode),
                    Ok(super::org::StorageMode::Schema)
                ) =>
            {
                return Err(TenancyError::Validation(format!(
                    "tenant `{slug}` is in schema mode, which reads no database URL; \
                     move it to database mode first"
                )));
            }
            ("database_url", SqlValue::String(new)) => {
                let url =
                    crate::tenancy::provision::vet_database_url(&new, secrets, registry, None)
                        .await?;
                crate::tenancy::provision::registry_alias(&url.resolved, registry).await?;
                // Compared resolved: re-sealing the same URL is not a rotation.
                let old = match existing.database_url.as_deref() {
                    Some(old) => secrets.resolve(old).await.ok(),
                    None => None,
                };
                database_url_rotated = old.as_deref() != Some(url.resolved.as_str());
                SqlValue::String(url.stored)
            }
            ("host_pattern" | "path_prefix", v) if blank(&v) => SqlValue::Null,
            ("host_pattern", SqlValue::String(v)) => {
                // Store the normalized form so it matches a `Host` header byte-for-byte.
                let host = crate::tenancy::provision::validate_host_pattern(&v)
                    .map_err(TenancyError::Validation)?;
                claims.push(ClaimKey::Host(host.clone()));
                SqlValue::String(host)
            }
            ("path_prefix", SqlValue::String(v)) => {
                let v = v.trim().to_owned();
                // A kept prefix passes: one set before a console route took it stays editable.
                if existing.path_prefix.as_deref() != Some(v.as_str()) {
                    crate::tenancy::provision::validate_new_path_prefix(&v)
                        .map_err(TenancyError::Validation)?;
                }
                claims.push(ClaimKey::PathPrefix(v.clone()));
                SqlValue::String(v)
            }
            ("port", SqlValue::I32(n)) => {
                crate::tenancy::provision::validate_port(n).map_err(TenancyError::Validation)?;
                claims.push(ClaimKey::Port(n));
                SqlValue::I32(n)
            }
            (_, v) => v,
        };
        if matches!(column, "host_pattern" | "path_prefix" | "port") {
            routing.push((column, value.clone()));
        }
        touched.push(column);
        set.push(assign(column, value));
    }

    if set.is_empty() {
        return Err(TenancyError::Validation(
            "nothing to change — pass at least one field".into(),
        ));
    }
    // The routing keys as the edit leaves them: one must remain.
    let after = |column: &str| routing.iter().find(|(c, _)| *c == column).map(|(_, v)| v);
    let text = |column: &str, kept: Option<&str>| match after(column) {
        Some(SqlValue::String(v)) => Some(v.clone()),
        Some(_) => None,
        None => kept.map(str::to_owned),
    };
    let host = text("host_pattern", existing.host_pattern.as_deref());
    let prefix = text("path_prefix", existing.path_prefix.as_deref());
    let port = match after("port") {
        Some(SqlValue::I32(n)) => Some(*n),
        Some(_) => None,
        None => existing.port,
    };
    // Only an edit that touches routing is checked, so a tenant made before
    // the rule (none set) can still be resealed or renamed.
    if !routing.is_empty()
        && !crate::tenancy::provision::is_routable(host.as_deref(), prefix.as_deref(), port)
    {
        return Err(TenancyError::Validation(
            crate::tenancy::provision::unroutable(slug),
        ));
    }

    let mut filters = vec![crate::core::Filter {
        column: "slug",
        op: crate::core::Op::Eq,
        value: crate::core::SqlValue::String(slug.to_owned()),
    }];
    if let Some(old) = expected {
        filters.push(crate::core::Filter {
            column: "database_url",
            op: crate::core::Op::Eq,
            value: crate::core::SqlValue::String(old.to_owned()),
        });
    }
    let update = crate::core::UpdateQuery {
        model: Org::SCHEMA,
        set,
        where_clause: crate::core::WhereExpr::and_predicates(filters),
    };
    let mut tx = if claims.is_empty() {
        crate::sql::write_transaction(registry).await?
    } else {
        claim_routing(registry, &claims, Claimant::Base(existing_id))
            .await
            .map_err(ClaimError::into_tenancy)?
    };
    if crate::sql::update_tx(&mut tx, &update).await? == 0 && expected.is_some() {
        return Ok(None);
    }
    if existing.active || activating {
        let forget = crate::tenancy::provision_store::forget_unsucceeded_runs(existing_id)?;
        crate::sql::update_tx(&mut tx, &forget).await?;
    }
    tx.commit().await?;

    // Before anything else acts on the write — see the module docs.
    crate::tenancy::invalidate_org_cache();

    Ok(Some(Applied {
        touched,
        database_url_rotated,
    }))
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

    /// A write over a `database_url` that changed since it was read is skipped.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_conditional_write_skips_a_changed_url() {
        let pool = Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::migrate_framework(&pool).await.unwrap();
        let first = "sqlite:///tmp/first.db?mode=rwc";
        let mut org = Org {
            slug: "cas".into(),
            backend_kind: "sqlite".into(),
            database_url: Some(first.into()),
            host_pattern: Some("cas.localhost".into()),
            ..crate::testkit::org()
        };
        org.save_pool(&pool).await.unwrap();
        let secrets = TenantSecrets::standard();
        let tmp = tempfile::tempdir().unwrap();
        let next = format!("sqlite://{}?mode=rwc", tmp.path().join("b.db").display());
        let set = || vec![("database_url", crate::core::SqlValue::String(next.clone()))];
        let stale = apply_values_if(&pool, &secrets, "cas", set(), Some("old"))
            .await
            .unwrap();
        assert!(stale.is_none());
        let rows: Vec<Org> = Org::objects().fetch(&pool).await.unwrap();
        assert_eq!(rows[0].database_url.as_deref(), Some(first));
        let fresh = apply_values_if(&pool, &secrets, "cas", set(), Some(first))
            .await
            .unwrap();
        assert!(fresh.is_some());
    }

    #[test]
    fn patch_debug_masks_the_database_password() {
        let p = OrgPatch {
            database_url: Some("postgres://u:db-pw-123@h/acme".into()),
            ..OrgPatch::default()
        };
        let s = format!("{p:?}");
        assert!(
            s.contains("database_url") && !s.contains("db-pw-123"),
            "{s}"
        );
    }

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
