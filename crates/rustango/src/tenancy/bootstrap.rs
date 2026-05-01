//! Built-in bootstrap migrations for the tenancy registry / tenant
//! tables.
//!
//! [`init_tenancy`] writes two scoped fixture migrations into the
//! operator's migrations directory:
//!
//! * `0001_rustango_registry_initial.json` — `scope: registry`,
//!   creates `rustango_orgs` + `rustango_operators` with `UNIQUE`
//!   constraints on `slug` / `username`.
//! * `0001_rustango_tenant_initial.json` — `scope: tenant`,
//!   creates `rustango_users` with `UNIQUE` on `username`.
//!
//! Both files exist already? `init_tenancy` skips them — so it's
//! idempotent and operators can re-run it safely after adding new
//! migrations.
//!
//! ## Why two files with the same numeric prefix
//!
//! Each scope has an independent migration chain (the scoped runner
//! filters by `scope` before walking `prev`). Both bootstraps are
//! "head" migrations in their respective chains — `prev: None`. They
//! share the `0001_` prefix because they're the operator's first
//! migration *in their scope*; the next operator-authored migration
//! gets `0002_…` regardless of scope.
//!
//! ## Why `make_migrations` doesn't re-emit these
//!
//! Both migrations carry a snapshot containing **all three** tenancy
//! tables (`rustango_orgs`, `rustango_operators`, `rustango_users`).
//! That overstates each migration's individual effect, but
//! [`rustango::migrate::make_migrations`] takes the lex-greatest
//! migration's snapshot as the prior baseline — and since the
//! tenant bootstrap (lex-greater than the registry one) has all
//! three tables in its snapshot, the diff against the registry
//! shows no missing tables. Operator-authored migrations land on
//! top with a clean diff.
//!
//! ## UNIQUE constraints
//!
//! `#[rustango(unique)]` doesn't exist yet (slated for a follow-up
//! ~1-day add). Until then, the bootstrap migrations carry the
//! UNIQUE constraints as raw `DataOp` SQL with a matching
//! `reverse_sql` so `unapply` works cleanly.

use std::path::Path;

use crate::core::Model as _;
use crate::migrate::{DataOp, Migration, MigrationScope, Operation, SchemaChange, SchemaSnapshot};

use super::auth::{Operator, User};
use super::auth_backends::ApiKey;
use super::error::TenancyError;
use super::org::Org;
use super::permissions::{Role, RolePermission, UserPermission, UserRole};

/// File stem of the registry-scoped bootstrap migration.
pub const REGISTRY_BOOTSTRAP_NAME: &str = "0001_rustango_registry_initial";
/// File stem of the tenant-scoped bootstrap migration.
pub const TENANT_BOOTSTRAP_NAME: &str = "0001_rustango_tenant_initial";

const BOOTSTRAP_TIMESTAMP: &str = "2026-04-29T00:00:00Z";

/// Build the registry-scoped bootstrap migration in memory.
#[must_use]
pub fn registry_bootstrap_migration() -> Migration {
    Migration {
        name: REGISTRY_BOOTSTRAP_NAME.to_owned(),
        created_at: BOOTSTRAP_TIMESTAMP.to_owned(),
        prev: None,
        atomic: true,
        scope: MigrationScope::Registry,
        snapshot: full_snapshot(),
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable("rustango_orgs".into())),
            Operation::Schema(SchemaChange::CreateTable(
                "rustango_operators".into(),
            )),
            Operation::Data(unique_constraint("rustango_orgs", "slug")),
            Operation::Data(unique_constraint("rustango_operators", "username")),
        ],
    }
}

/// Build the tenant-scoped bootstrap migration in memory.
#[must_use]
pub fn tenant_bootstrap_migration() -> Migration {
    Migration {
        name: TENANT_BOOTSTRAP_NAME.to_owned(),
        created_at: BOOTSTRAP_TIMESTAMP.to_owned(),
        prev: None,
        atomic: true,
        scope: MigrationScope::Tenant,
        snapshot: full_snapshot(),
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable("rustango_users".into())),
            Operation::Data(unique_constraint("rustango_users", "username")),
        ],
    }
}

/// Snapshot containing **all three** tenancy tables, regardless of
/// scope. Both bootstrap migrations share this snapshot so the
/// lex-greatest one (`0001_rustango_tenant_initial`) leaves the
/// world looking complete to a downstream `make_migrations`.
fn full_snapshot() -> SchemaSnapshot {
    SchemaSnapshot::from_models(&[
        Org::SCHEMA,
        Operator::SCHEMA,
        User::SCHEMA,
        Role::SCHEMA,
        RolePermission::SCHEMA,
        UserRole::SCHEMA,
        UserPermission::SCHEMA,
        ApiKey::SCHEMA,
    ])
}

fn unique_constraint(table: &str, column: &str) -> DataOp {
    DataOp {
        sql: format!(
            r#"ALTER TABLE "{table}" ADD CONSTRAINT "{table}_{column}_key" UNIQUE ("{column}")"#,
        ),
        reverse_sql: Some(format!(
            r#"ALTER TABLE "{table}" DROP CONSTRAINT "{table}_{column}_key""#,
        )),
        reversible: true,
    }
}

/// Outcome of [`init_tenancy`]: which files were written and which
/// were skipped because they already existed.
#[derive(Debug, Default)]
pub struct InitTenancyReport {
    /// Migration names that were freshly written into `dir`.
    pub written: Vec<String>,
    /// Migration names that already existed in `dir` and were left
    /// untouched.
    pub skipped: Vec<String>,
}

/// Materialize the bootstrap migrations into `dir`. Idempotent:
/// existing files (matched by name) are skipped without rewriting.
///
/// `dir` is created if missing.
///
/// # Errors
/// Returns [`TenancyError::Migrate`] when the directory can't be
/// created or a file can't be written, or [`TenancyError::Io`] for
/// raw I/O failures elsewhere.
pub fn init_tenancy(dir: &Path) -> Result<InitTenancyReport, TenancyError> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let mut report = InitTenancyReport::default();
    for mig in [registry_bootstrap_migration(), tenant_bootstrap_migration()] {
        let path = dir.join(format!("{}.json", mig.name));
        if path.exists() {
            report.skipped.push(mig.name);
            continue;
        }
        rustango::migrate::file::write(&path, &mig)?;
        report.written.push(mig.name);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_migration_is_registry_scoped() {
        let m = registry_bootstrap_migration();
        assert_eq!(m.scope, MigrationScope::Registry);
        assert_eq!(m.name, REGISTRY_BOOTSTRAP_NAME);
        assert!(m.prev.is_none());
        // Two CreateTable ops for orgs + operators, plus 2 UNIQUE DataOps.
        assert_eq!(m.forward.len(), 4);
    }

    #[test]
    fn tenant_migration_is_tenant_scoped() {
        let m = tenant_bootstrap_migration();
        assert_eq!(m.scope, MigrationScope::Tenant);
        assert_eq!(m.name, TENANT_BOOTSTRAP_NAME);
        assert!(m.prev.is_none());
        // CreateTable + UNIQUE DataOp.
        assert_eq!(m.forward.len(), 2);
    }

    #[test]
    fn snapshots_contain_all_three_tenancy_tables() {
        let s = full_snapshot();
        let names: Vec<&str> = s.tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"rustango_orgs"));
        assert!(names.contains(&"rustango_operators"));
        assert!(names.contains(&"rustango_users"));
    }

    #[test]
    fn unique_constraint_emits_alter_table_pair() {
        let op = unique_constraint("rustango_orgs", "slug");
        assert!(op.sql.contains("ADD CONSTRAINT \"rustango_orgs_slug_key\""));
        assert!(op.sql.contains("UNIQUE (\"slug\")"));
        assert!(op.reversible);
        let reverse = op.reverse_sql.unwrap();
        assert!(reverse.contains("DROP CONSTRAINT \"rustango_orgs_slug_key\""));
    }

    #[test]
    fn init_tenancy_writes_then_skips_idempotently() {
        let dir = tempdir();
        let first = init_tenancy(&dir).unwrap();
        assert_eq!(first.written.len(), 2);
        assert!(first.skipped.is_empty());

        // Both files now exist on disk.
        assert!(dir.join(format!("{REGISTRY_BOOTSTRAP_NAME}.json")).exists());
        assert!(dir.join(format!("{TENANT_BOOTSTRAP_NAME}.json")).exists());

        let second = init_tenancy(&dir).unwrap();
        assert!(second.written.is_empty());
        assert_eq!(second.skipped.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("rustango_tenancy_bootstrap_test_{pid}_{n}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }
}
