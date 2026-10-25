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
    if std::env::var(crate::aead::KEY_VAR).is_err() {
        return Err(TenancyError::Validation(format!(
            "{} is not set, so nothing can be sealed",
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
