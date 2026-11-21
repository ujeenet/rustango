//! `check-tenants` and `repair-tenant`: find and fix a `rustango_orgs` row
//! whose stored storage mode or backend this version cannot read.
//!
//! Such a row fails every read of it, so both verbs read the two columns
//! as raw text. Repair only rewrites a cell that is unreadable or on an
//! alias; it never moves a tenant to another mode or backend.

use std::io::Write;

use sqlx::Database;

use crate::core::{Column as _, SqlValue, TextEnum};
use crate::tenancy::error::TenancyError;
use crate::tenancy::org::{BackendKind, Org, StorageMode};
use crate::tenancy::pools::TenantPools;

use super::args::{parse, Spec};

const REPAIR: Spec<'static> = Spec {
    verb: "repair-tenant",
    usage: "repair-tenant <slug> [--storage-mode <schema|database>] \
            [--backend <postgres|mysql|sqlite>]",
    switches: &[],
    valued: &["--storage-mode", "--backend"],
    max_positionals: 1,
};

/// `check-tenants`: list unreadable cells (an error) and alias cells (a hint).
pub(super) async fn check_tenants<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let check = Org::check_stored_values(&pools.registry_pool()).await?;
    for row in &check.aliases {
        writeln!(
            w,
            "alias: tenant `{}` stores {} as `{}`; it reads fine, `repair-tenant {}` rewrites it",
            row.slug, row.column, row.value, row.slug
        )?;
    }
    for row in &check.bad {
        writeln!(w, "unreadable: {row}")?;
    }
    if check.bad.is_empty() {
        writeln!(w, "every tenant row reads")?;
        return Ok(());
    }
    Err(TenancyError::Validation(format!(
        "{} tenant value(s) cannot be read; fix each with `repair-tenant <slug> \
         --storage-mode <m>` or `--backend <b>`",
        check.bad.len()
    )))
}

/// `repair-tenant <slug>`: rewrite unreadable or alias cells as stored names.
pub(super) async fn repair_tenant<W: Write + Send, DB: Database>(
    pools: &TenantPools<DB>,
    args: &[String],
    w: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::sql::UpdaterPool as _;
    let p = parse(args, &REPAIR)?;
    let slug = p.required(0, REPAIR.usage)?;
    let registry = pools.registry_pool();
    let rows = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .values_list(&["id", "storage_mode", "backend_kind", "schema_name"])
        .fetch(&registry)
        .await?;
    let Some([SqlValue::I64(id), mode, backend, schema_name]) = rows.first().map(Vec::as_slice)
    else {
        return Err(TenancyError::Validation(format!("no tenant `{slug}`")));
    };
    let mode = repaired::<StorageMode>("storage_mode", mode, p.value("--storage-mode")?)?;
    let backend = repaired::<BackendKind>("backend_kind", backend, p.value("--backend")?)?;
    backend
        .value
        .validate_storage_mode(mode.value)
        .map_err(|e| TenancyError::Validation(e.to_owned()))?;
    if !mode.changed && !backend.changed {
        writeln!(w, "tenant `{slug}` already stores canonical values")?;
        return Ok(());
    }
    // As provisioning: two schema-mode tenants never share one schema.
    if mode.changed && mode.value == StorageMode::Schema {
        let named = match schema_name {
            SqlValue::String(s) => Some(s.as_str()),
            _ => None,
        };
        let schema = crate::tenancy::org::effective_schema(named, &slug);
        if crate::tenancy::org_host::schema_claimed(&registry, schema, Some(*id)).await? {
            return Err(TenancyError::Validation(format!(
                "another tenant uses schema `{schema}`; repair-tenant will not share it"
            )));
        }
    }
    Org::objects()
        .where_(Org::id.eq(*id))
        .update()
        .set_typed(Org::storage_mode.set(mode.value))
        .set_typed(Org::backend_kind.set(backend.value))
        .execute_pool(&registry)
        .await?;
    crate::tenancy::invalidate_org_cache();
    writeln!(
        w,
        "repaired tenant `{slug}`: storage_mode `{}`, backend_kind `{}`",
        mode.value, backend.value
    )?;
    Ok(())
}

#[derive(Debug)]
struct Repaired<E> {
    value: E,
    changed: bool,
}

/// The value to store for one cell: what it reads as, or `flag` when it
/// reads as nothing. A flag never changes a readable cell to another variant.
fn repaired<E: TextEnum>(
    column: &str,
    stored: &SqlValue,
    flag: Option<&str>,
) -> Result<Repaired<E>, TenancyError> {
    let text = match stored {
        SqlValue::String(s) => s.as_str(),
        _ => "",
    };
    let wanted = flag.map(E::from_db).transpose()?;
    match (E::from_db(text), wanted) {
        (Ok(v), Some(w)) if v != w => Err(TenancyError::Validation(format!(
            "{column} reads as `{v}`, so repair-tenant leaves it; moving a tenant to \
             another mode or backend does not move its data (see migrate-tenant-storage)",
            v = v.as_str()
        ))),
        (Ok(v), _) => Ok(Repaired {
            value: v,
            changed: v.as_str() != text,
        }),
        (Err(_), Some(w)) => Ok(Repaired {
            value: w,
            changed: true,
        }),
        (Err(e), None) => Err(TenancyError::Validation(format!(
            "{column}: {e}; pass the value it should hold"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> SqlValue {
        SqlValue::String(s.to_owned())
    }

    #[test]
    fn repair_rewrites_bad_and_alias_cells_only() {
        let r = repaired::<BackendKind>("backend_kind", &text("pg"), None).unwrap();
        assert!(r.changed && r.value == BackendKind::Postgres);
        let r = repaired::<BackendKind>("backend_kind", &text("postgres"), None).unwrap();
        assert!(!r.changed);
        let r = repaired::<StorageMode>("storage_mode", &text("Schema"), Some("schema")).unwrap();
        assert!(r.changed && r.value == StorageMode::Schema);
        let e = repaired::<StorageMode>("storage_mode", &text("Schema"), None).unwrap_err();
        assert!(e.to_string().contains("pass the value"), "{e}");
        let e =
            repaired::<StorageMode>("storage_mode", &text("schema"), Some("database")).unwrap_err();
        assert!(e.to_string().contains("does not move its data"), "{e}");
    }
}
