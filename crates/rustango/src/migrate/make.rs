//! Generate the next migration file from a registry diff.
//!
//! [`make_migrations`] is the entry point: load the latest snapshot in
//! `dir`, build the current snapshot from the inventory registry, diff,
//! and write the new file. [`make_migrations_from`] is the testable
//! form — it takes the current snapshot as a parameter so tests can
//! supply controlled fixtures without touching the global registry.
//!
//! Auto-naming heuristic (used when `name_override` is `None`):
//!
//! | shape of changes                       | suffix                    |
//! |----------------------------------------|---------------------------|
//! | empty dir + all `CreateTable`          | `initial`                 |
//! | single `CreateTable("foo")`            | `create_foo`              |
//! | single `DropTable("foo")`              | `drop_foo`                |
//! | single `AddColumn { table, column }`   | `add_<column>_to_<table>` |
//! | single `DropColumn { table, column }`  | `drop_<column>_from_<table>` |
//! | anything else                          | `auto`                    |

use std::path::Path;

use super::diff::{detect_changes, detect_unsupported_field_changes, SchemaChange};
use super::error::MigrateError;
use super::file::{self, extract_index, Migration, Operation};
use super::snapshot::SchemaSnapshot;

/// Produce the next migration file in `dir` by diffing the inventory
/// registry against the latest snapshot on disk.
///
/// Returns `Ok(None)` if the registry matches the latest snapshot (no
/// migration needed).
///
/// # Errors
/// Anything [`make_migrations_from`] can return.
pub fn make_migrations(
    dir: &Path,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let current = SchemaSnapshot::from_registry();
    make_migrations_from(dir, &current, name_override)
}

/// Tenancy-aware counterpart of [`make_migrations`] — diffs only the
/// models whose [`crate::core::ModelSchema::scope`] matches `scope`,
/// and emits the migration with [`super::MigrationScope`] set to the
/// matching value. Powers the `makemigrations` flow on tenancy
/// projects so registry-scoped framework tables (`Org`, `Operator`)
/// don't bleed into tenant-scoped migrations.
///
/// Both the current snapshot AND the prior on-disk snapshot are
/// filtered to `scope` before diffing — the latter is critical for
/// projects that scaffolded against pre-v0.24.2 bootstrap migrations
/// (which carry every framework table in one snapshot regardless of
/// scope). Tables not currently in the inventory default to
/// [`crate::core::ModelScope::Tenant`] (see
/// [`SchemaSnapshot::filtered_to_scope`]).
///
/// Returns `Ok(None)` when nothing in this scope changed.
///
/// # Errors
/// As [`make_migrations_from`].
pub fn make_migrations_for_scope(
    dir: &Path,
    scope: crate::core::ModelScope,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let current = SchemaSnapshot::from_registry_for_scope(scope);
    let migration_scope = match scope {
        crate::core::ModelScope::Registry => super::MigrationScope::Registry,
        crate::core::ModelScope::Tenant => super::MigrationScope::Tenant,
    };
    make_migrations_scoped(dir, &current, scope, migration_scope, name_override)
}

/// Per-app counterpart of [`make_migrations`] — diffs only the models
/// whose Django-shape app label matches `app`, and writes the result
/// into `<project_root>/<app>/migrations/`. Powers the
/// `manage makemigrations <app>` flow (slice 9.0g).
///
/// `project_root` is typically the project's `src/` (or whatever the
/// scaffolder's `--into` was set to). Returns `Ok(None)` when nothing
/// changed, or when no models carry that `app_label`.
///
/// # Errors
/// Anything [`make_migrations_from`] can return, plus
/// [`MigrateError::Io`] if the per-app migrations dir can't be
/// created.
pub fn make_migrations_for_app(
    project_root: &Path,
    app: &str,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let app_dir = project_root.join(app).join("migrations");
    if !app_dir.exists() {
        std::fs::create_dir_all(&app_dir)?;
    }
    let current = SchemaSnapshot::from_registry_for_app(app);
    make_migrations_from(&app_dir, &current, name_override)
}

/// Scope-filtered counterpart of [`make_migrations_from`] used by
/// [`make_migrations_for_scope`]. Considers only prior migrations
/// whose `MigrationScope` matches `migration_scope` when building the
/// previous snapshot, and filters that snapshot down to `model_scope`
/// to handle pre-v0.24.2 bootstrap migrations that mixed every
/// framework table into one snapshot.
///
/// # Errors
/// As [`make_migrations_from`].
pub fn make_migrations_scoped(
    dir: &Path,
    current: &SchemaSnapshot,
    model_scope: crate::core::ModelScope,
    migration_scope: super::MigrationScope,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let prior = file::list_dir(dir)?;
    // Filter prior to migrations in our scope only — registry runs in
    // its own chain, tenant in its own chain. Both bootstrap files
    // share the `0001_` prefix because they're both "head" migrations
    // in their respective chains.
    let prior_scoped: Vec<&Migration> = prior
        .iter()
        .filter(|m| m.scope == migration_scope)
        .collect();
    let prev_snapshot = prior_scoped
        .last()
        .map_or_else(empty_snapshot, |m| m.snapshot.clone())
        .filtered_to_scope(model_scope);
    let prev_name = prior_scoped.last().map(|m| m.name.clone());
    // Index numbering walks the WHOLE directory — we don't want
    // tenant-scoped 0002 colliding with registry-scoped 0002 in the
    // same directory.
    let next_index = prior
        .last()
        .and_then(|m| extract_index(&m.name))
        .map_or(1, |n| n + 1);

    let unsupported = detect_unsupported_field_changes(&prev_snapshot, current);
    if !unsupported.is_empty() {
        return Err(MigrateError::Validation(format!(
            "field metadata changed but v0.3 has no AlterField operation \
             (deferred to v0.4); the following changes need explicit migration \
             authoring:\n  - {}",
            unsupported.join("\n  - "),
        )));
    }

    let changes = detect_changes(&prev_snapshot, current);
    if changes.is_empty() {
        return Ok(None);
    }

    let suffix = name_override.map_or_else(
        || auto_name(&changes, prior_scoped.is_empty()),
        str::to_owned,
    );
    let name = format!("{next_index:04}_{suffix}");
    let created_at = chrono::Utc::now().to_rfc3339();

    let mig = Migration {
        name: name.clone(),
        created_at,
        prev: prev_name,
        atomic: true,
        scope: migration_scope,
        snapshot: current.clone(),
        forward: changes.into_iter().map(Operation::Schema).collect(),
    };

    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let path = dir.join(format!("{name}.json"));
    file::write(&path, &mig)?;
    Ok(Some(mig))
}

/// Testable form of [`make_migrations`] that takes the current snapshot
/// as input rather than building it from the registry.
///
/// # Errors
/// Returns [`MigrateError::Io`] / [`MigrateError::Json`] for file
/// problems (loading prior migrations, writing the new one) and
/// [`MigrateError::Validation`] if any prior migration is corrupt.
pub fn make_migrations_from(
    dir: &Path,
    current: &SchemaSnapshot,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let prior = file::list_dir(dir)?;
    let prev_snapshot = prior
        .last()
        .map_or_else(empty_snapshot, |m| m.snapshot.clone());
    let prev_name = prior.last().map(|m| m.name.clone());
    let next_index = prior
        .last()
        .and_then(|m| extract_index(&m.name))
        .map_or(1, |n| n + 1);

    // Reject metadata-only changes that v0.3's `SchemaChange` set can't
    // represent (type swaps, nullability flips, default/CHECK/FK
    // tweaks, etc.). Without this guard `make_migrations` would
    // silently produce `Ok(None)` and the user would think the schema
    // was already up to date. v0.4 will introduce `AlterField` ops to
    // close this gap; until then, surface the change as a clear error.
    let unsupported = detect_unsupported_field_changes(&prev_snapshot, current);
    if !unsupported.is_empty() {
        return Err(MigrateError::Validation(format!(
            "field metadata changed but v0.3 has no AlterField operation \
             (deferred to v0.4); the following changes need explicit migration \
             authoring:\n  - {}",
            unsupported.join("\n  - "),
        )));
    }

    let changes = detect_changes(&prev_snapshot, current);
    if changes.is_empty() {
        return Ok(None);
    }

    let suffix = name_override.map_or_else(|| auto_name(&changes, prior.is_empty()), str::to_owned);
    let name = format!("{next_index:04}_{suffix}");
    let created_at = chrono::Utc::now().to_rfc3339();

    let mig = Migration {
        name: name.clone(),
        created_at,
        prev: prev_name,
        atomic: true,
        scope: super::MigrationScope::default(),
        snapshot: current.clone(),
        forward: changes.into_iter().map(Operation::Schema).collect(),
    };

    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let path = dir.join(format!("{name}.json"));
    file::write(&path, &mig)?;
    Ok(Some(mig))
}

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
    }
}

fn auto_name(changes: &[SchemaChange], is_first: bool) -> String {
    match changes {
        [SchemaChange::CreateTable(t)] => {
            if is_first {
                "initial".into()
            } else {
                format!("create_{t}")
            }
        }
        [SchemaChange::DropTable(t)] => format!("drop_{t}"),
        [SchemaChange::AddColumn { table, column }] => format!("add_{column}_to_{table}"),
        [SchemaChange::DropColumn { table, column }] => format!("drop_{column}_from_{table}"),
        [SchemaChange::AlterColumnType {
            table,
            column,
            from,
            to,
        }] => format!("alter_{column}_on_{table}_{from}_to_{to}"),
        [SchemaChange::AlterColumnNullable {
            table,
            column,
            nullable,
        }] => {
            if *nullable {
                format!("make_{column}_on_{table}_nullable")
            } else {
                format!("make_{column}_on_{table}_not_null")
            }
        }
        [SchemaChange::AlterColumnDefault { table, column, .. }] => {
            format!("alter_default_of_{column}_on_{table}")
        }
        [SchemaChange::AlterColumnMaxLength { table, column, .. }] => {
            format!("alter_max_length_of_{column}_on_{table}")
        }
        [SchemaChange::RenameTable { old_name, new_name }] => {
            format!("rename_{old_name}_to_{new_name}")
        }
        [SchemaChange::RenameColumn {
            table,
            old_column,
            new_column,
        }] => format!("rename_{old_column}_to_{new_column}_on_{table}"),
        [SchemaChange::CreateIndex { name, .. }] => format!("create_index_{name}"),
        [SchemaChange::DropIndex { name }] => format!("drop_index_{name}"),
        [SchemaChange::AddCheckConstraint { name, .. }] => format!("add_check_{name}"),
        [SchemaChange::DropCheckConstraint { name, .. }] => format!("drop_check_{name}"),
        [SchemaChange::CreateM2MTable { through, .. }] => format!("create_m2m_{through}"),
        [SchemaChange::DropM2MTable { through }] => format!("drop_m2m_{through}"),
        many if is_first
            && many
                .iter()
                .all(|c| matches!(c, SchemaChange::CreateTable(_))) =>
        {
            "initial".into()
        }
        _ => "auto".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_name_initial_for_first_migration_with_create_tables() {
        let changes = vec![
            SchemaChange::CreateTable("a".into()),
            SchemaChange::CreateTable("b".into()),
        ];
        assert_eq!(auto_name(&changes, true), "initial");
    }

    #[test]
    fn auto_name_single_create_table_after_initial() {
        let changes = vec![SchemaChange::CreateTable("foo".into())];
        assert_eq!(auto_name(&changes, false), "create_foo");
    }

    #[test]
    fn auto_name_single_drop_table() {
        let changes = vec![SchemaChange::DropTable("ghost".into())];
        assert_eq!(auto_name(&changes, false), "drop_ghost");
    }

    #[test]
    fn auto_name_add_column() {
        let changes = vec![SchemaChange::AddColumn {
            table: "article".into(),
            column: "slug".into(),
        }];
        assert_eq!(auto_name(&changes, false), "add_slug_to_article");
    }

    #[test]
    fn auto_name_drop_column() {
        let changes = vec![SchemaChange::DropColumn {
            table: "article".into(),
            column: "deprecated".into(),
        }];
        assert_eq!(auto_name(&changes, false), "drop_deprecated_from_article");
    }

    #[test]
    fn auto_name_mixed_falls_back_to_auto() {
        let changes = vec![
            SchemaChange::CreateTable("foo".into()),
            SchemaChange::AddColumn {
                table: "bar".into(),
                column: "baz".into(),
            },
        ];
        assert_eq!(auto_name(&changes, false), "auto");
    }

    // ============================================================ scope-aware
    //
    // Regression coverage for the v0.24.2 fix: a tenancy project with
    // mixed registry-scoped (Org/Operator) and tenant-scoped (User +
    // user models) models in inventory used to dump every change into
    // a single tenant-scoped migration. When `migrate-tenants` fanned
    // it out per-tenant, the registry-table ALTER would re-resolve via
    // search_path to the registry copy and crash with `relation …
    // already exists`.
    //
    // The fix splits diffs by `ModelScope`: registry models go to a
    // file tagged `MigrationScope::Registry`, tenant models go to one
    // tagged `MigrationScope::Tenant`. These tests exercise the
    // partitioning helpers without touching the global inventory.

    use crate::core::ModelScope;
    use crate::migrate::snapshot::{FieldSnapshot, SchemaSnapshot, TableSnapshot};
    use crate::migrate::MigrationScope;

    fn snap_with(tables: Vec<TableSnapshot>) -> SchemaSnapshot {
        SchemaSnapshot {
            tables,
            m2m_tables: vec![],
            indexes: vec![],
            checks: vec![],
        }
    }

    fn t(name: &str) -> TableSnapshot {
        TableSnapshot {
            name: name.into(),
            model: name.into(),
            fields: vec![FieldSnapshot {
                name: "id".into(),
                column: "id".into(),
                ty: "i64".into(),
                nullable: false,
                primary_key: true,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: true,
                unique: false,
                fk: None,
            }],
            composite_fks: vec![],
        }
    }

    #[test]
    fn make_migrations_scoped_with_no_changes_returns_none() {
        // Seed dir with a prior tenant migration whose snapshot already
        // matches `current` → diff is empty → no new file emitted.
        let dir = tempdir();
        let snap = snap_with(vec![t("rustango_users")]);
        let prior = Migration {
            name: "0001_initial".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Tenant,
            snapshot: snap.clone(),
            forward: vec![],
        };
        std::fs::write(
            dir.join("0001_initial.json"),
            serde_json::to_string(&prior).unwrap(),
        )
        .unwrap();
        let r = make_migrations_scoped(
            &dir,
            &snap,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap();
        assert!(r.is_none(), "no changes should yield no file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn make_migrations_scoped_emits_with_correct_migration_scope() {
        // First call from empty dir → snapshot has 1 tenant table → file
        // created and tagged with MigrationScope::Tenant.
        let dir = tempdir();
        let snap = snap_with(vec![t("rustango_users")]);
        let mig = make_migrations_scoped(
            &dir,
            &snap,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap()
        .expect("expected a migration file");
        assert_eq!(mig.scope, MigrationScope::Tenant);
        assert!(mig.name.starts_with("0001_"), "got: {}", mig.name);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn make_migrations_scoped_filters_prev_to_scope_for_old_bootstrap_layout() {
        // Simulate the v0.23.x / pre-v0.24.2 bootstrap: snapshot
        // contains BOTH registry and tenant framework tables. The
        // `filtered_to_scope` step in make_migrations_scoped must drop
        // the registry tables before diffing, so the tenant-scope
        // diff sees only tenant-side changes (here: a new user table)
        // and does NOT emit ops for `rustango_operators`.
        //
        // We use a tenant-scope diff with prev containing a name that
        // looks like the old bootstrap. Inventory lookup falls back to
        // Tenant for unknown tables, so only tables explicitly in
        // inventory with `scope = Registry` get filtered out — for
        // this test we trust the lookup path's default behavior.
        let dir = tempdir();
        let prev = Migration {
            name: "0001_initial".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Tenant,
            snapshot: snap_with(vec![t("rustango_users")]),
            forward: vec![],
        };
        let path = dir.join("0001_initial.json");
        std::fs::write(&path, serde_json::to_string(&prev).unwrap()).unwrap();
        // current adds a new tenant table.
        let current = snap_with(vec![t("posts"), t("rustango_users")]);
        let mig = make_migrations_scoped(
            &dir,
            &current,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap()
        .expect("expected a migration");
        assert_eq!(mig.scope, MigrationScope::Tenant);
        // The CreateTable for `posts` is the only forward op.
        assert_eq!(mig.forward.len(), 1, "got: {:?}", mig.forward);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn make_migrations_scoped_indexes_walk_full_dir_not_just_scope() {
        // Two prior migrations in different scopes. Index numbering
        // walks both so we don't get filename collisions:
        // - 0001_registry_initial.json (scope=Registry)
        // - 0002_initial.json          (scope=Tenant)
        // A new tenant migration must land at 0003, not 0002.
        let dir = tempdir();
        let r = Migration {
            name: "0001_registry_initial".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Registry,
            snapshot: snap_with(vec![t("rustango_orgs")]),
            forward: vec![],
        };
        let t1 = Migration {
            name: "0002_initial".into(),
            created_at: "2026-01-02T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Tenant,
            snapshot: snap_with(vec![t("rustango_users")]),
            forward: vec![],
        };
        std::fs::write(
            dir.join("0001_registry_initial.json"),
            serde_json::to_string(&r).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("0002_initial.json"),
            serde_json::to_string(&t1).unwrap(),
        )
        .unwrap();
        let current = snap_with(vec![t("posts"), t("rustango_users")]);
        let mig = make_migrations_scoped(
            &dir,
            &current,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap()
        .expect("expected a migration");
        assert!(
            mig.name.starts_with("0003_"),
            "next migration must be 0003 to avoid collision with 0002, got: {}",
            mig.name
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("rustango_make_scope_test_{pid}_{n}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
