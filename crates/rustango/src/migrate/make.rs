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
    // v0.31.1 (#2): the chain head's snapshot is the obvious baseline,
    // but it's incomplete when the project has multiple "head"
    // migrations in the same scope. Concrete case: `init-tenancy`
    // writes `0001_rustango_tenant_initial` (tenant scope, no `prev`)
    // alongside the user's `0001_initial`. Subsequent user-app
    // migrations chain off `0001_initial` and carry forward only the
    // user-app tables in their snapshots — the framework tables drop
    // out of the baseline. Diffing against the inventory then re-emits
    // `CreateTable` for every framework table and the migration runner
    // crashes with `relation already exists`.
    //
    // Two-step fix:
    //   1. Merge any side-chain bootstrap snapshots (no `prev`, not in
    //      the main chain) into the baseline.
    //   2. Pre-populate the baseline with every `rustango_*` table the
    //      current registry knows about. The framework reserves the
    //      `rustango_` table-name prefix for tables it manages itself
    //      (bootstrap migrations + lazy ensure-table paths like
    //      `audit_log`, `content_types`, `permissions`). User-app
    //      makemigrations should never emit CreateTable for those.
    let mut prev_snapshot = prior_scoped
        .last()
        .map_or_else(empty_snapshot, |m| m.snapshot.clone());
    let in_chain = chain_membership(&prior_scoped);
    for m in &prior_scoped {
        if !in_chain.contains(m.name.as_str()) {
            fold_in_missing_tables(&mut prev_snapshot, &m.snapshot);
        }
    }
    fold_in_framework_tables(&mut prev_snapshot, current);
    let prev_snapshot = prev_snapshot.filtered_to_scope(model_scope);
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

/// Generate the framework's own **system-app** migrations into
/// `<project_root>/system/migrations/`.
///
/// Diffs the framework (`rustango_*`) models of `scope` against the
/// prior system migrations of that scope and writes a scope-tagged
/// migration. Unlike the user path ([`make_migrations_scoped`]) this
/// does NOT fold framework tables into the baseline — here they ARE the
/// subject, so a fresh project emits `CreateTable` for every framework
/// table and later runs emit `AddColumn`/`DropColumn` as the models (and
/// their `#[cfg]` features) evolve. This is what replaces the
/// hand-written bootstrap/ensure DDL.
///
/// Returns `Ok(None)` when nothing in the framework's schema changed for
/// this scope.
///
/// # Errors
/// As [`make_migrations_from`], plus [`MigrateError::Io`] if the
/// `system/migrations/` dir can't be created.
pub fn make_migrations_system(
    project_root: &Path,
    scope: crate::core::ModelScope,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let dir = project_root.join("system").join("migrations");
    let migration_scope = match scope {
        crate::core::ModelScope::Registry => super::MigrationScope::Registry,
        crate::core::ModelScope::Tenant => super::MigrationScope::Tenant,
    };
    let current = SchemaSnapshot::from_registry_system_for_scope(scope);
    let prior = if dir.exists() {
        file::list_dir(&dir)?
    } else {
        Vec::new()
    };
    let prior_scoped: Vec<&Migration> = prior
        .iter()
        .filter(|m| m.scope == migration_scope)
        .collect();
    let prev_snapshot = prior_scoped
        .last()
        .map_or_else(empty_snapshot, |m| m.snapshot.clone());
    let prev_name = prior_scoped.last().map(|m| m.name.clone());
    let next_index = prior
        .last()
        .and_then(|m| extract_index(&m.name))
        .map_or(1, |n| n + 1);

    let unsupported = detect_unsupported_field_changes(&prev_snapshot, &current);
    if !unsupported.is_empty() {
        return Err(MigrateError::Validation(format!(
            "framework schema change needs an operation the engine can't yet emit \
             (author manually or wait for the AlterField op):\n  - {}",
            unsupported.join("\n  - "),
        )));
    }
    let changes = detect_changes(&prev_snapshot, &current);
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
        std::fs::create_dir_all(&dir)?;
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

/// Names reachable from the lex-last in-scope migration by walking
/// `prev` links backward. Migrations whose names are NOT in this set
/// are side-chain bootstrap migrations whose tables would otherwise
/// drop out of the baseline. See `make_migrations_scoped` for the
/// `0001_rustango_tenant_initial` collision this guards against (#2).
fn chain_membership(prior_scoped: &[&Migration]) -> std::collections::HashSet<String> {
    let mut seen = std::collections::HashSet::new();
    let Some(last) = prior_scoped.last() else {
        return seen;
    };
    let mut cur: Option<&str> = Some(last.name.as_str());
    while let Some(name) = cur {
        if !seen.insert(name.to_owned()) {
            // Defensive — a cyclic `prev` chain shouldn't happen but
            // would loop forever if it did.
            break;
        }
        cur = prior_scoped
            .iter()
            .find(|m| m.name == name)
            .and_then(|m| m.prev.as_deref());
    }
    seen
}

/// Pre-populate `prev_snapshot` with every `rustango_*` table the
/// current inventory knows about. The `rustango_` table-name prefix is
/// a reserved framework namespace — the framework creates those
/// tables itself (via bootstrap migrations or lazy ensure-table
/// paths). User-app makemigrations diffs should treat them as
/// already-present so they don't get re-emitted as CreateTable ops
/// that crash on `relation already exists` (#2).
fn fold_in_framework_tables(into: &mut SchemaSnapshot, current: &SchemaSnapshot) {
    for t in &current.tables {
        if t.name.starts_with("rustango_") && !into.tables.iter().any(|x| x.name == t.name) {
            into.tables.push(t.clone());
        }
    }
    for m2m in &current.m2m_tables {
        if m2m.through.starts_with("rustango_")
            && !into.m2m_tables.iter().any(|x| x.through == m2m.through)
        {
            into.m2m_tables.push(m2m.clone());
        }
    }
    for idx in &current.indexes {
        if idx.table.starts_with("rustango_") && !into.indexes.iter().any(|x| x.name == idx.name) {
            into.indexes.push(idx.clone());
        }
    }
    for c in &current.checks {
        if c.table.starts_with("rustango_") && !into.checks.iter().any(|x| x.name == c.name) {
            into.checks.push(c.clone());
        }
    }
}

/// Add every table / m2m / index / check from `from` to `into` that
/// isn't already named in `into`. Used to fold side-chain bootstrap
/// snapshots into the chain head's baseline (#2). The "missing-only"
/// semantics keep in-chain DropTable operations honored.
fn fold_in_missing_tables(into: &mut SchemaSnapshot, from: &SchemaSnapshot) {
    for t in &from.tables {
        if !into.tables.iter().any(|x| x.name == t.name) {
            into.tables.push(t.clone());
        }
    }
    for m2m in &from.m2m_tables {
        if !into.m2m_tables.iter().any(|x| x.through == m2m.through) {
            into.m2m_tables.push(m2m.clone());
        }
    }
    for idx in &from.indexes {
        if !into.indexes.iter().any(|x| x.name == idx.name) {
            into.indexes.push(idx.clone());
        }
    }
    for c in &from.checks {
        if !into.checks.iter().any(|x| x.name == c.name) {
            into.checks.push(c.clone());
        }
    }
}

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
        excludes: vec![],
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
        // v0.31.1: previously this case fell through to the
        // unhelpful "auto" — generating uninformative filenames like
        // `0004_auto.json` even when the diff was a clean set of
        // `CreateTable`s + their indexes. Now: if every op is a
        // CreateTable, or a CreateIndex targeting one of those new
        // tables, name the migration after the tables created
        // (capped at 3, joined with `_and_`).
        many if many.iter().all(|c| {
            matches!(
                c,
                SchemaChange::CreateTable(_)
                    | SchemaChange::CreateIndex { .. }
                    | SchemaChange::CreateM2MTable { .. }
            )
        }) =>
        {
            let mut tables: Vec<&str> = many
                .iter()
                .filter_map(|c| match c {
                    SchemaChange::CreateTable(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            tables.sort_unstable();
            tables.dedup();
            if tables.is_empty() {
                "auto".into()
            } else if tables.len() <= 3 {
                format!("create_{}", tables.join("_and_"))
            } else {
                format!("create_{}_etc", tables[..3].join("_and_"))
            }
        }
        _ => "auto".into(),
    }
}

/// Django-shape `makemigrations --merge` — issue #346. Reconcile
/// a divergent migration history by writing an empty-forward
/// "merge" file whose `prev` points at the lex-last leaf, so the
/// next `makemigrations` writes a proper linear successor.
///
/// A *leaf* is a migration whose name is NOT referenced as another
/// migration's `prev`. With a single branch the chain has exactly
/// one leaf (the latest file). Two engineers each running
/// `makemigrations` on a feature branch produce a second leaf
/// pointing at the same parent — when their PRs both merge to
/// main, the resulting tree has two leaves and the next
/// `makemigrations` would arbitrarily pick one as its `prev`,
/// silently losing the convergence point.
///
/// This function snapshots the current model registry, writes a
/// new `NNNN_merge.json` whose `prev` is the lex-last leaf with
/// `forward: []`, and returns the written migration. Re-merging an
/// already-linear chain returns `Ok(None)` and writes nothing.
///
/// # Errors
/// Returns [`MigrateError::Validation`] if a chain prerequisite
/// fails (broken `prev` link, missing PK index), or
/// [`MigrateError::Io`] / [`MigrateError::Json`] on file I/O.
pub fn make_merge_migration(dir: &Path) -> Result<Option<Migration>, MigrateError> {
    let current = SchemaSnapshot::from_registry();
    make_merge_migration_from(dir, &current)
}

/// Testable form of [`make_merge_migration`] — takes the post-merge
/// snapshot as input rather than building it from the registry.
///
/// # Errors
/// See [`make_merge_migration`].
pub fn make_merge_migration_from(
    dir: &Path,
    current: &SchemaSnapshot,
) -> Result<Option<Migration>, MigrateError> {
    let prior = file::list_dir(dir)?;
    if prior.is_empty() {
        return Err(MigrateError::Validation(
            "makemigrations --merge: no migrations in directory — nothing to merge".into(),
        ));
    }

    // A leaf is any migration NOT referenced as anyone else's `prev`.
    let referenced: std::collections::HashSet<&str> =
        prior.iter().filter_map(|m| m.prev.as_deref()).collect();
    let mut leaves: Vec<&Migration> = prior
        .iter()
        .filter(|m| !referenced.contains(m.name.as_str()))
        .collect();
    leaves.sort_by(|a, b| a.name.cmp(&b.name));

    if leaves.len() < 2 {
        // Chain is already linear — nothing to merge.
        return Ok(None);
    }

    // Reject leaves whose own `prev`s point at different ancestors —
    // those represent *legitimately diverging* histories, not branch
    // collisions. Django's --merge guards the same case.
    let parents: std::collections::HashSet<Option<String>> =
        leaves.iter().map(|m| m.prev.clone()).collect();
    if parents.len() > 1 {
        let names: Vec<String> = leaves.iter().map(|m| m.name.clone()).collect();
        return Err(MigrateError::Validation(format!(
            "makemigrations --merge: leaves don't share a common parent — \
             can't auto-reconcile. Leaves: {}",
            names.join(", ")
        )));
    }

    // `prev` for the merge node = the lex-last leaf so the chain is
    // unambiguous from this point forward. Lex-sort apply order
    // already runs the other leaves first, so applying this merge
    // file (empty `forward`) is a no-op at runtime — its only job
    // is to anchor the chain for future `makemigrations`.
    let merge_prev = leaves.last().unwrap().name.clone();

    // Pick the next sequential prefix from the highest existing
    // index across the whole directory, not just within the leaves.
    let next_index = prior
        .iter()
        .filter_map(|m| extract_index(&m.name))
        .max()
        .map_or(1, |n| n + 1);

    let name = format!("{next_index:04}_merge");
    let created_at = chrono::Utc::now().to_rfc3339();

    let mig = Migration {
        name: name.clone(),
        created_at,
        prev: Some(merge_prev),
        atomic: true,
        scope: super::MigrationScope::default(),
        // Snapshot reflects the post-merge cumulative schema. The
        // registry's current state IS that snapshot — by the time
        // the operator runs `--merge`, the checkout has both
        // branches' model changes compiled in. Future
        // `makemigrations` will diff against this snapshot.
        snapshot: current.clone(),
        forward: Vec::new(),
    };

    let path = dir.join(format!("{name}.json"));
    file::write(&path, &mig)?;
    Ok(Some(mig))
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
            excludes: vec![],
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
                case_insensitive: false,
                generated_as: None,
                db_comment: None,
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
        //
        // v0.31.1 (#2): table name must NOT start with `rustango_` —
        // that prefix is reserved for framework-managed tables which
        // are now filtered out of the user-app diff baseline. Use a
        // user-app-shape name (`posts`) so this test exercises the
        // "first migration created" path.
        let dir = tempdir();
        let snap = snap_with(vec![t("posts")]);
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

    // ============================================================ #346
    //
    // makemigrations --merge — reconcile a divergent chain by writing an
    // empty-forward `NNNN_merge.json` whose `prev` points at the lex-last
    // leaf. The merge file's snapshot is the cumulative post-merge
    // schema (the `current` snapshot supplied at merge time).

    fn write_mig(dir: &std::path::Path, mig: &Migration) {
        std::fs::write(
            dir.join(format!("{}.json", mig.name)),
            serde_json::to_string_pretty(mig).unwrap(),
        )
        .unwrap();
    }

    fn mig_at(name: &str, prev: Option<&str>, snapshot: SchemaSnapshot) -> Migration {
        Migration {
            name: name.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            prev: prev.map(str::to_owned),
            atomic: true,
            scope: MigrationScope::Tenant,
            snapshot,
            forward: vec![],
        }
    }

    #[test]
    fn merge_returns_none_when_chain_is_linear() {
        let dir = tempdir();
        let snap = snap_with(vec![t("posts")]);
        write_mig(&dir, &mig_at("0001_initial", None, snap.clone()));
        write_mig(
            &dir,
            &mig_at("0002_add", Some("0001_initial"), snap.clone()),
        );
        let r = make_merge_migration_from(&dir, &snap).unwrap();
        assert!(
            r.is_none(),
            "single-leaf chain should NOT emit a merge file, got {r:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_errors_when_dir_is_empty() {
        let dir = tempdir();
        let snap = snap_with(vec![t("posts")]);
        let err = make_merge_migration_from(&dir, &snap).unwrap_err();
        assert!(matches!(err, MigrateError::Validation(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_writes_merge_file_when_two_leaves_share_parent() {
        let dir = tempdir();
        let snap = snap_with(vec![t("posts")]);
        // 0001 ← 0002a (leaf A) and 0001 ← 0002b (leaf B).
        write_mig(&dir, &mig_at("0001_initial", None, snap.clone()));
        write_mig(
            &dir,
            &mig_at("0002a_branch", Some("0001_initial"), snap.clone()),
        );
        write_mig(
            &dir,
            &mig_at("0002b_branch", Some("0001_initial"), snap.clone()),
        );

        let mig = make_merge_migration_from(&dir, &snap)
            .unwrap()
            .expect("two leaves should produce a merge file");

        // Next index = max(0001, 0002a, 0002b) = 2 → next is 0003.
        assert!(
            mig.name.starts_with("0003_"),
            "merge file index should follow lex-last leaf, got: {}",
            mig.name
        );
        assert_eq!(mig.name, "0003_merge");
        // Lex-last leaf wins prev — alphabetically 0002b > 0002a.
        assert_eq!(mig.prev.as_deref(), Some("0002b_branch"));
        // No schema changes — the merge file just anchors the chain.
        assert!(mig.forward.is_empty(), "merge file must have empty forward");
        // Re-running on the now-linearized chain (0001 ← 0002a, 0002b ← 0003_merge)
        // returns None since 0002a is the only remaining leaf, but wait —
        // 0002a was never linked to either by `prev`. After the merge it's
        // STILL a leaf because nothing references it. So another merge file
        // would still want to be written.
        //
        // The Django-shape behavior is: lex-sort applies 0002a BEFORE 0002b BEFORE
        // 0003_merge so all schema changes from both branches apply in order.
        // The merge file's only job is to give the next `makemigrations` a
        // single unambiguous parent. Once 0003_merge is written, 0003_merge
        // is the new lex-last leaf. 0002a is also still a "leaf" (no
        // dependents) — but that's a graph degenerate case, not a chain
        // conflict, so this test stops at the first merge write.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_rejects_leaves_with_different_parents() {
        let dir = tempdir();
        let snap = snap_with(vec![t("posts")]);
        // Two leaves with different parents — legitimately divergent
        // history, not a branch-collision. Django's --merge does the
        // same.
        write_mig(&dir, &mig_at("0001_a", None, snap.clone()));
        write_mig(&dir, &mig_at("0002_b", Some("0001_a"), snap.clone()));
        write_mig(&dir, &mig_at("0001_z", None, snap.clone()));
        let err = make_merge_migration_from(&dir, &snap).unwrap_err();
        match err {
            MigrateError::Validation(msg) => {
                assert!(
                    msg.contains("common parent"),
                    "error should mention common-parent requirement, got: {msg}"
                );
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_uses_supplied_snapshot_for_post_merge_state() {
        let dir = tempdir();
        // Pre-existing leaves carry a snapshot with one table; the
        // post-merge `current` snapshot has TWO. The merge file should
        // record the post-merge snapshot so future `makemigrations`
        // diffs against the correct baseline.
        let pre = snap_with(vec![t("posts")]);
        let post = snap_with(vec![t("posts"), t("comments")]);
        write_mig(&dir, &mig_at("0001_initial", None, pre.clone()));
        write_mig(&dir, &mig_at("0002a", Some("0001_initial"), pre.clone()));
        write_mig(&dir, &mig_at("0002b", Some("0001_initial"), pre));

        let mig = make_merge_migration_from(&dir, &post).unwrap().unwrap();
        assert_eq!(
            mig.snapshot.tables.len(),
            2,
            "merge file snapshot must reflect the post-merge schema"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
