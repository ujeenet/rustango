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
    SchemaSnapshot { tables: vec![], m2m_tables: vec![] }
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
}
