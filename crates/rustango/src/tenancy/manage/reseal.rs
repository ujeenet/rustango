//! `reseal-tenant-secrets` — re-seal stored tenant URLs, server passwords,
//! operator TOTP and SSO secrets with the current
//! `RUSTANGO_SECRET_KEY`, after a rotation (old key in
//! `RUSTANGO_SECRET_KEY_PREVIOUS`) or to seal URLs stored before a key was set.
//! Dry run by default; `--apply` writes.

use std::io::Write;

use sqlx::Database;

use crate::core::SqlValue;
use crate::sql::connect_diagnosis::{redact, SEALED_SCHEME};
use crate::sql::FetcherPool as _;
use crate::tenancy::db_server::{self, Resealed};
use crate::tenancy::error::TenancyError;
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
    // Operator TOTP secrets, bound to their operator: without them a
    // rotation locks every enrolled operator out of the console.
    #[cfg(feature = "totp")]
    {
        use crate::tenancy::operator_totp::{self, OperatorTotp};
        let rows = OperatorTotp::objects()
            .values_list(&["operator_id", "secret", "pending_secret"])
            .fetch(&registry)
            .await
            .map_err(|e| e.to_string());
        match rows {
            // Created on the first enrollment: none yet, nothing to reseal.
            Err(e) if missing_table(&e) => {}
            Err(e) => {
                failed += 1;
                writeln!(w, "  operator totp: cannot open: {e}")?;
            }
            Ok(rows) => {
                for row in rows {
                    let [SqlValue::I64(id), secret, pending] = row.as_slice() else {
                        continue;
                    };
                    for (column, value) in [("secret", secret), ("pending_secret", pending)] {
                        let SqlValue::String(stored) = value else {
                            continue;
                        };
                        todo += 1;
                        let what = format!("operator totp {id}/{column}");
                        if !apply {
                            writeln!(w, "  {what}: would re-seal")?;
                            continue;
                        }
                        let outcome = match operator_totp::reseal(*id, stored) {
                            Ok(sealed) => {
                                let cell = SealedCell {
                                    model: <OperatorTotp as crate::core::Model>::SCHEMA,
                                    pk: ("operator_id", *id),
                                    column,
                                    stored: stored.clone(),
                                };
                                cell.write(&registry, sealed).await
                            }
                            Err(e) => Err(format!("cannot open: {e}")),
                        };
                        match outcome {
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
        }
    }
    // SSO client secrets: the shared set, then each tenant's own.
    #[cfg(feature = "sso")]
    {
        let mut tables: Vec<(String, Result<Vec<SsoSecret>, String>, crate::sql::Pool)> =
            Vec::new();
        #[cfg(feature = "admin-sso")]
        {
            use crate::tenancy::sso::SharedSsoProvider as P;
            let rows = SsoSecret::list(
                P::objects().values_list(SsoSecret::COLUMNS),
                <P as crate::core::Model>::SCHEMA,
                &registry,
            )
            .await;
            tables.push(("shared".into(), rows, registry.clone()));
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
            let rows = SsoSecret::list(
                P::objects().values_list(SsoSecret::COLUMNS),
                <P as crate::core::Model>::SCHEMA,
                &pool,
            )
            .await;
            tables.push((org.slug, rows, pool));
        }
        for (owner, rows, pool) in tables {
            let rows = match rows {
                Ok(r) => r,
                // A tenant made before the table existed has nothing to reseal.
                Err(e) if missing_table(&e) => continue,
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
        writeln!(
            w,
            "reseal-tenant-secrets: {todo} to change (dry run; pass --apply)"
        )?;
    }
    if failed > 0 {
        return Err(TenancyError::Validation(format!(
            "{failed} secret(s) could not be resealed"
        )));
    }
    Ok(())
}

/// A driver error for a table that is not there, on any backend.
#[cfg(any(feature = "sso", feature = "totp"))]
fn missing_table(e: &str) -> bool {
    e.contains("does not exist") || e.contains("no such table") || e.contains("doesn't exist")
}

/// One sealed value as stored, keyed by an integer primary key.
#[cfg(any(feature = "sso", feature = "totp"))]
struct SealedCell {
    model: &'static crate::core::ModelSchema,
    pk: (&'static str, i64),
    column: &'static str,
    stored: String,
}

#[cfg(any(feature = "sso", feature = "totp"))]
impl SealedCell {
    /// Write `sealed` only while the cell still holds what was read.
    /// `Ok(false)`: it changed meanwhile.
    async fn write(&self, pool: &crate::sql::Pool, sealed: String) -> Result<bool, String> {
        use crate::core::{Assignment, Filter, Op, UpdateQuery, WhereExpr};
        let query = UpdateQuery {
            model: self.model,
            set: vec![Assignment {
                column: self.column,
                value: SqlValue::String(sealed).into(),
            }],
            where_clause: WhereExpr::and_predicates(vec![
                Filter {
                    column: self.pk.0,
                    op: Op::Eq,
                    value: SqlValue::I64(self.pk.1),
                },
                Filter {
                    column: self.column,
                    op: Op::Eq,
                    value: SqlValue::String(self.stored.clone()),
                },
            ]),
        };
        crate::sql::update_pool(pool, &query)
            .await
            .map(|n| n > 0)
            .map_err(|e| e.to_string())
    }
}

/// One SSO provider's client secret as stored, sealed.
#[cfg(feature = "sso")]
struct SsoSecret {
    id: i64,
    slug: String,
    stored: String,
    model: &'static crate::core::ModelSchema,
}

#[cfg(feature = "sso")]
impl SsoSecret {
    const COLUMNS: &'static [&'static str] = &["id", "slug", "client_secret"];

    /// The rows of `rows`, raw: the compare-and-set needs the stored text.
    async fn list<T: crate::core::Model>(
        rows: crate::query::ValuesListQuerySet<T>,
        model: &'static crate::core::ModelSchema,
        pool: &crate::sql::Pool,
    ) -> Result<Vec<Self>, String> {
        let rows = rows.fetch(pool).await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .filter_map(|r| match r.as_slice() {
                [SqlValue::I64(id), SqlValue::String(slug), SqlValue::String(stored)] => {
                    Some(Self {
                        id: *id,
                        slug: slug.clone(),
                        stored: stored.clone(),
                        model,
                    })
                }
                _ => None,
            })
            .collect())
    }

    /// Write it sealed under the current key, only while the stored value
    /// is the one read. `Ok(false)`: it changed meanwhile.
    async fn reseal(&self, pool: &crate::sql::Pool) -> Result<bool, String> {
        let plain =
            crate::aead::decrypt(&self.stored, b"").map_err(|e| format!("cannot open: {e:?}"))?;
        let sealed =
            crate::aead::encrypt(&plain, b"").map_err(|e| format!("cannot seal: {e:?}"))?;
        let cell = SealedCell {
            model: self.model,
            pk: ("id", self.id),
            column: "client_secret",
            stored: self.stored.clone(),
        };
        cell.write(pool, sealed).await
    }
}
