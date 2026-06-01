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
//! `#[rustango(unique)]` emits `UNIQUE` inline on the column DDL so
//! no separate `DataOp` constraint is needed. Both bootstrap migrations
//! use only `CreateTable` ops; the unique constraints come from the
//! model field attributes (`Org.slug`, `Operator.username`,
//! `User.username`, `Role.name`).

use std::path::Path;

use crate::core::Model as _;
use crate::migrate::{Migration, MigrationScope, Operation, SchemaChange, SchemaSnapshot};

use super::auth::{validate_tenant_user_schema, Operator, TenantUserModel, User};
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
    registry_bootstrap_migration_for::<User>()
}

/// Build the tenant-scoped bootstrap migration in memory.
#[must_use]
pub fn tenant_bootstrap_migration() -> Migration {
    tenant_bootstrap_migration_for::<User>()
}

/// Like [`registry_bootstrap_migration`] but uses `U`'s schema for
/// `rustango_users`. Both bootstrap migrations carry the full
/// snapshot, so the registry one needs the override too — otherwise
/// `make_migrations` would see drift between the registry and tenant
/// snapshots and try to "fix" the user table.
///
/// # Panics
/// If `U`'s schema doesn't satisfy [`validate_tenant_user_schema`]
/// (wrong table name or missing a required column).
#[must_use]
pub fn registry_bootstrap_migration_for<U: TenantUserModel>() -> Migration {
    Migration {
        name: REGISTRY_BOOTSTRAP_NAME.to_owned(),
        created_at: BOOTSTRAP_TIMESTAMP.to_owned(),
        prev: None,
        atomic: true,
        scope: MigrationScope::Registry,
        snapshot: full_snapshot_for::<U>(),
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable("rustango_orgs".into())),
            Operation::Schema(SchemaChange::CreateTable("rustango_operators".into())),
        ],
    }
}

/// Like [`tenant_bootstrap_migration`] but uses `U`'s schema for
/// `rustango_users`, so any extra columns declared on `U` land in
/// the bootstrap `CREATE TABLE`.
///
/// # Panics
/// If `U`'s schema doesn't satisfy [`validate_tenant_user_schema`].
#[must_use]
pub fn tenant_bootstrap_migration_for<U: TenantUserModel>() -> Migration {
    Migration {
        name: TENANT_BOOTSTRAP_NAME.to_owned(),
        created_at: BOOTSTRAP_TIMESTAMP.to_owned(),
        prev: None,
        atomic: true,
        scope: MigrationScope::Tenant,
        snapshot: full_snapshot_for::<U>(),
        // #315 — `full_snapshot_for` declares the roles/permissions
        // tables, so the forward ops must actually CREATE them too.
        // Previously only `rustango_users` was created, leaving the
        // snapshot claiming five tables that never existed; any
        // downstream migration that trusts the snapshot and FKs to them
        // then fails at apply time on FK-enforcing dialects (MySQL err
        // 1824) and silently leaves them missing on SQLite. FKs are
        // deferred by the runner, so creation order is irrelevant.
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable("rustango_users".into())),
            Operation::Schema(SchemaChange::CreateTable("rustango_roles".into())),
            Operation::Schema(SchemaChange::CreateTable(
                "rustango_role_permissions".into(),
            )),
            Operation::Schema(SchemaChange::CreateTable("rustango_user_roles".into())),
            Operation::Schema(SchemaChange::CreateTable(
                "rustango_user_permissions".into(),
            )),
            Operation::Schema(SchemaChange::CreateTable("rustango_api_keys".into())),
        ],
    }
}

/// Snapshot containing **all three** tenancy tables, regardless of
/// scope. Both bootstrap migrations share this snapshot so the
/// lex-greatest one (`0001_rustango_tenant_initial`) leaves the
/// world looking complete to a downstream `make_migrations`.
fn full_snapshot_for<U: TenantUserModel>() -> SchemaSnapshot {
    if let Err(e) = validate_tenant_user_schema(&U::SCHEMA) {
        panic!("invalid TenantUserModel: {e}");
    }
    SchemaSnapshot::from_models(&[
        Org::SCHEMA,
        Operator::SCHEMA,
        U::SCHEMA,
        Role::SCHEMA,
        RolePermission::SCHEMA,
        UserRole::SCHEMA,
        UserPermission::SCHEMA,
        ApiKey::SCHEMA,
    ])
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
    init_tenancy_with::<User>(dir)
}

/// Like [`init_tenancy`] but uses `U`'s schema for the
/// `rustango_users` table — any extra columns on `U` are written into
/// the materialized bootstrap migration JSON.
///
/// Idempotent: only writes files that don't already exist. The
/// override therefore matters only on the first invocation; once the
/// JSON is on disk it's the source of truth and changing `U` won't
/// rewrite it (write a follow-up `AddColumn` migration instead).
///
/// # Errors
/// As [`init_tenancy`].
///
/// # Panics
/// If `U`'s schema doesn't satisfy
/// [`super::auth::validate_tenant_user_schema`].
pub fn init_tenancy_with<U: TenantUserModel>(
    dir: &Path,
) -> Result<InitTenancyReport, TenancyError> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let mut report = InitTenancyReport::default();
    for mig in [
        registry_bootstrap_migration_for::<U>(),
        tenant_bootstrap_migration_for::<U>(),
    ] {
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
        // Two CreateTable ops: rustango_orgs + rustango_operators.
        // UNIQUE constraints are now inline on the column (via #[rustango(unique)]).
        assert_eq!(m.forward.len(), 2);
    }

    #[test]
    fn tenant_migration_is_tenant_scoped() {
        let m = tenant_bootstrap_migration();
        assert_eq!(m.scope, MigrationScope::Tenant);
        assert_eq!(m.name, TENANT_BOOTSTRAP_NAME);
        assert!(m.prev.is_none());
        // One CreateTable op for rustango_users.
        // UNIQUE constraint on username is now inline via #[rustango(unique)].
        assert_eq!(m.forward.len(), 1);
    }

    #[test]
    fn snapshots_contain_all_three_tenancy_tables() {
        let s = full_snapshot_for::<User>();
        let names: Vec<&str> = s.tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"rustango_orgs"));
        assert!(names.contains(&"rustango_operators"));
        assert!(names.contains(&"rustango_users"));
    }

    /// Custom user model with an extra column. Mirrors every required
    /// framework column so the validator passes.
    #[derive(crate::Model, Debug, Clone)]
    #[rustango(table = "rustango_users")]
    #[allow(dead_code)]
    pub struct AppUserExt {
        #[rustango(primary_key)]
        pub id: rustango::sql::Auto<i64>,
        #[rustango(max_length = 64, unique)]
        pub username: String,
        #[rustango(max_length = 255)]
        pub password_hash: String,
        pub is_superuser: bool,
        pub active: bool,
        pub created_at: chrono::DateTime<chrono::Utc>,
        #[rustango(default = "'{}'")]
        pub data: serde_json::Value,
        #[rustango(max_length = 128, default = "''")]
        pub display_name: String,
        #[rustango(max_length = 64, default = "'UTC'")]
        pub timezone: String,
    }

    impl super::TenantUserModel for AppUserExt {}

    #[test]
    fn user_model_override_lands_extras_in_tenant_snapshot() {
        let m = tenant_bootstrap_migration_for::<AppUserExt>();
        let users = m
            .snapshot
            .table("rustango_users")
            .expect("rustango_users in snapshot");
        let cols: Vec<&str> = users.fields.iter().map(|f| f.column.as_str()).collect();
        // framework defaults still present
        for required in super::super::auth::REQUIRED_USER_COLUMNS {
            assert!(
                cols.contains(required),
                "missing required column {required}"
            );
        }
        // extras present
        assert!(cols.contains(&"display_name"), "extras must be in snapshot");
        assert!(cols.contains(&"timezone"));
    }

    #[test]
    fn user_model_override_also_propagates_to_registry_snapshot() {
        // Both bootstrap migrations share the same snapshot — the
        // registry one must carry the extras too, otherwise
        // make_migrations would diff the two and try to "fix" the
        // discrepancy on the next run.
        let m = registry_bootstrap_migration_for::<AppUserExt>();
        let users = m.snapshot.table("rustango_users").unwrap();
        let cols: Vec<&str> = users.fields.iter().map(|f| f.column.as_str()).collect();
        assert!(cols.contains(&"display_name"));
    }

    #[test]
    fn default_user_snapshot_has_no_extras() {
        let m = tenant_bootstrap_migration();
        let users = m.snapshot.table("rustango_users").unwrap();
        let cols: Vec<&str> = users.fields.iter().map(|f| f.column.as_str()).collect();
        // Regression: stock User must not gain extras silently.
        assert!(!cols.contains(&"display_name"));
        assert!(!cols.contains(&"timezone"));
    }

    #[test]
    fn init_tenancy_with_writes_override_migration() {
        let dir = tempdir();
        let report = init_tenancy_with::<AppUserExt>(&dir).unwrap();
        assert_eq!(report.written.len(), 2);

        // Read the tenant bootstrap JSON back and confirm extras
        // round-tripped through serialization.
        let path = dir.join(format!("{TENANT_BOOTSTRAP_NAME}.json"));
        let mig = rustango::migrate::file::load(&path).unwrap();
        let users = mig.snapshot.table("rustango_users").unwrap();
        let cols: Vec<&str> = users.fields.iter().map(|f| f.column.as_str()).collect();
        assert!(cols.contains(&"display_name"));

        let _ = std::fs::remove_dir_all(&dir);
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
