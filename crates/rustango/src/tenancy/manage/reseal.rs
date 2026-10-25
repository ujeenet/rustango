//! `reseal-tenant-secrets` — re-seal stored tenant URLs with the current
//! `RUSTANGO_SECRET_KEY`, after a rotation (old key in
//! `RUSTANGO_SECRET_KEY_PREVIOUS`) or to seal URLs stored before a key was set.
//! Dry run by default; `--apply` writes.

use std::io::Write;

use sqlx::Database;

use crate::sql::connect_diagnosis::{redact, SEALED_SCHEME};
use crate::sql::FetcherPool as _;
use crate::tenancy::error::TenancyError;
use crate::core::SqlValue;
use crate::tenancy::db_server::{self, Resealed};
use crate::tenancy::org_edit::apply_values_if;
use crate::tenancy::pools::TenantPools;
use crate::tenancy::{Org, SecretsResolver as _};

pub(super) async fn reseal_tenant_secrets<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let apply = match args {
        [] => false,
        [flag] if flag == "--apply" => true,
        _ => {
            return Err(TenancyError::Validation(
                "usage: reseal-tenant-secrets [--apply]".into(),
            ))
        }
    };
    if crate::aead::key_status() == crate::aead::KeyStatus::Missing {
        return Err(TenancyError::Validation(format!(
            "{} is not set or blank, so nothing can be sealed",
            crate::aead::KEY_VAR
        )));
    }
    // Never write a value back unsealed: that would downgrade an `enc:` row.
    let secrets = pools.secrets().clone().require_sealed();
    let registry = pools.registry_pool();
    let mut orgs: Vec<Org> = Org::objects().fetch(&registry).await?;
    orgs.sort_by(|a, b| a.slug.cmp(&b.slug));

    let (mut todo, mut done, mut skipped, mut failed) = (0usize, 0usize, 0usize, 0usize);
    for org in orgs {
        let Some(stored) = org.database_url.as_deref() else {
            continue;
        };
        let sealed = stored.starts_with(SEALED_SCHEME);
        let resolved = match secrets.resolve(stored).await {
            Ok(url) => url,
            Err(e) if sealed => {
                failed += 1;
                writeln!(w, "  {}: cannot open: {e}", org.slug)?;
                continue;
            }
            // Not ours to touch: an `env://` or vault reference that does not resolve here.
            Err(_) => continue,
        };
        let has_password = redact(&resolved) != resolved;
        let literal_with_password = resolved == stored && has_password;
        // A sealed value with no password would be written back in plain text.
        if (sealed && !has_password) || (!sealed && !literal_with_password) {
            continue;
        }
        todo += 1;
        let action = if sealed { "re-seal" } else { "seal" };
        if !apply {
            writeln!(w, "  {}: would {action}", org.slug)?;
            continue;
        }
        let values = vec![("database_url", SqlValue::String(resolved))];
        // Only over the value read above: a concurrent edit wins.
        match apply_values_if(&registry, &secrets, &org.slug, values, Some(stored)).await {
            Ok(Some(_)) => {
                done += 1;
                writeln!(w, "  {}: {action}ed", org.slug)?;
            }
            Ok(None) => {
                skipped += 1;
                writeln!(w, "  {}: skipped, changed meanwhile", org.slug)?;
            }
            Err(e) => {
                failed += 1;
                writeln!(w, "  {}: failed: {e}", org.slug)?;
            }
        }
    }
    // Saved server admin passwords: always sealed, so each one is re-sealed.
    for server in db_server::list(&registry).await? {
        if !server.has_password() {
            continue;
        }
        todo += 1;
        let name = &server.name;
        match db_server::reseal_password(&registry, &server, apply).await {
            Resealed::Would => writeln!(w, "  server {name}: would re-seal")?,
            Resealed::Done => {
                done += 1;
                writeln!(w, "  server {name}: re-sealed")?;
            }
            Resealed::Skipped => {
                skipped += 1;
                writeln!(w, "  server {name}: skipped, changed meanwhile")?;
            }
            Resealed::Failed(e) => {
                failed += 1;
                writeln!(w, "  server {name}: failed: {e}")?;
            }
        }
    }
    // SSO client secrets: the shared set, then each tenant's own.
    #[cfg(feature = "sso")]
    {
        let mut tables: Vec<(String, Result<Vec<SsoSecret>, String>, crate::sql::Pool)> = Vec::new();
        #[cfg(feature = "admin-sso")]
        {
            use crate::tenancy::sso::SharedSsoProvider as P;
            let rows = P::objects().fetch(&registry).await.map(|rows: Vec<P>| {
                rows.into_iter()
                    .map(|r| SsoSecret::of(r.id.get().copied(), r.updated_at.get().copied(), &r.slug, r.client_secret.into_inner(), <P as crate::core::Model>::SCHEMA))
                    .collect()
            });
            tables.push(("shared".into(), rows.map_err(|e| e.to_string()), registry.clone()));
        }
        let mut orgs: Vec<Org> = Org::objects().fetch(&registry).await?;
        orgs.sort_by(|a, b| a.slug.cmp(&b.slug));
        for org in orgs {
            use crate::sso::SsoProvider as P;
            let Ok(pool) = pools.scoped_pool_dyn(&org).await else {
                failed += 1;
                writeln!(w, "  sso {}: cannot reach the tenant", org.slug)?;
                continue;
            };
            let rows = P::objects().fetch(&pool).await.map(|rows: Vec<P>| {
                rows.into_iter()
                    .map(|r| SsoSecret::of(r.id.get().copied(), r.updated_at.get().copied(), &r.slug, r.client_secret.into_inner(), <P as crate::core::Model>::SCHEMA))
                    .collect()
            });
            tables.push((org.slug, rows.map_err(|e| e.to_string()), pool));
        }
        for (owner, rows, pool) in tables {
            let rows = match rows {
                Ok(r) => r,
                // A tenant made before the table existed has nothing to reseal.
                Err(e) if e.contains("does not exist") || e.contains("no such table") || e.contains("doesn't exist") => continue,
                Err(e) => {
                    failed += 1;
                    writeln!(w, "  sso {owner}: cannot open: {e}")?;
                    continue;
                }
            };
            for row in rows {
                todo += 1;
                let what = format!("sso {owner}/{}", row.slug);
                if !apply {
                    writeln!(w, "  {what}: would re-seal")?;
                    continue;
                }
                match row.reseal(&pool).await {
                    Ok(true) => {
                        done += 1;
                        writeln!(w, "  {what}: re-sealed")?;
                    }
                    Ok(false) => {
                        skipped += 1;
                        writeln!(w, "  {what}: skipped, changed meanwhile")?;
                    }
                    Err(e) => {
                        failed += 1;
                        writeln!(w, "  {what}: failed: {e}")?;
                    }
                }
            }
        }
    }
    if apply {
        writeln!(
            w,
            "reseal-tenant-secrets: {done} done, {skipped} skipped, {failed} failed"
        )?;
    } else {
        writeln!(w, "reseal-tenant-secrets: {todo} to change (dry run; pass --apply)")?;
    }
    if failed > 0 {
        return Err(TenancyError::Validation(format!(
            "{failed} secret(s) could not be resealed"
        )));
    }
    Ok(())
}

/// One SSO provider's client secret, opened (current or previous key).
#[cfg(feature = "sso")]
struct SsoSecret {
    id: i64,
    seen: Option<chrono::DateTime<chrono::Utc>>,
    slug: String,
    plain: String,
    model: &'static crate::core::ModelSchema,
}

#[cfg(feature = "sso")]
impl SsoSecret {
    fn of(
        id: Option<i64>,
        seen: Option<chrono::DateTime<chrono::Utc>>,
        slug: &str,
        plain: String,
        model: &'static crate::core::ModelSchema,
    ) -> Self {
        Self {
            id: id.unwrap_or_default(),
            seen,
            slug: slug.to_owned(),
            plain,
            model,
        }
    }

    /// Write it sealed under the current key, only while the row is as
    /// read. `Ok(false)`: it changed meanwhile.
    async fn reseal(&self, pool: &crate::sql::Pool) -> Result<bool, String> {
        use crate::core::{Assignment, Filter, Op, UpdateQuery, WhereExpr};
        let sealed = crate::aead::encrypt(self.plain.as_bytes(), b"")
            .map_err(|e| format!("cannot seal: {e:?}"))?;
        let seen = match self.seen {
            Some(t) => Filter {
                column: "updated_at",
                op: Op::Eq,
                value: SqlValue::DateTime(t),
            },
            None => Filter {
                column: "updated_at",
                op: Op::IsNull,
                value: SqlValue::Bool(true),
            },
        };
        let query = UpdateQuery {
            model: self.model,
            set: vec![Assignment {
                column: "client_secret",
                value: SqlValue::String(sealed).into(),
            }],
            where_clause: WhereExpr::and_predicates(vec![
                Filter {
                    column: "id",
                    op: Op::Eq,
                    value: SqlValue::I64(self.id),
                },
                seen,
            ]),
        };
        crate::sql::update_pool(pool, &query)
            .await
            .map(|n| n > 0)
            .map_err(|e| e.to_string())
    }
}
