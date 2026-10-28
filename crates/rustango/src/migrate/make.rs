//! Generate the next migration file from a registry diff.
//!
//! [`make_migrations`] is the entry point. It loads the latest
//! snapshot in `dir`, builds the current one from the model registry,
//! diffs them and writes a new file. [`make_migrations_from`] takes
//! the current snapshot as an argument, so a test can pass a fixture
//! instead of using the global registry.
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
    super::snapshot::check_registry_junctions()?;
    let current = SchemaSnapshot::from_registry();
    make_migrations_from(dir, &current, name_override)
}

/// [`make_migrations`] for one scope. Diffs only the models whose
/// [`crate::core::ModelSchema::scope`] matches, and tags the new file
/// with the matching [`super::MigrationScope`]. This keeps registry
/// tables such as `Org` out of tenant-scoped migrations.
///
/// Both the current snapshot and the prior on-disk one are filtered
/// to `scope` first. Filtering the prior one matters: older bootstrap
/// migrations hold every framework table in one snapshot whatever
/// their scope. A table no longer in the inventory counts as
/// [`crate::core::ModelScope::Tenant`]; see
/// [`SchemaSnapshot::filtered_to_scope`].
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
    super::snapshot::check_registry_junctions()?;
    let current = SchemaSnapshot::from_registry_for_scope(scope);
    let migration_scope = match scope {
        crate::core::ModelScope::Registry => super::MigrationScope::Registry,
        crate::core::ModelScope::Tenant => super::MigrationScope::Tenant,
    };
    make_migrations_scoped(dir, &current, scope, migration_scope, name_override)
}

/// [`make_migrations`] for one app. Diffs only the models whose app
/// label is `app` and writes to `<project_root>/<app>/migrations/`.
/// This backs `manage makemigrations <app>`.
///
/// `project_root` is usually the project's `src/`. Returns `Ok(None)`
/// when nothing changed or no model carries that app label.
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
    super::snapshot::check_registry_junctions()?;
    let current = SchemaSnapshot::from_registry_for_app(app);
    make_migrations_from(&app_dir, &current, name_override)
}

/// [`make_migrations_from`] for one scope, used by
/// [`make_migrations_for_scope`]. Builds the previous snapshot from
/// prior migrations of the same `migration_scope`, then filters it to
/// `model_scope` so an older bootstrap migration holding every
/// framework table does not pollute the diff.
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
    // Each scope has its own chain, so keep only this scope's prior
    // migrations. Both chains start at `0001_`, since each has its
    // own head.
    let prior_scoped: Vec<&Migration> = prior
        .iter()
        .filter(|m| m.scope == migration_scope)
        .collect();
    refuse_branched(&prior_scoped)?;
    // The last migration's snapshot is the obvious baseline, but it
    // is incomplete when a scope has more than one head. For example
    // `init-tenancy` writes its own `0001_` beside the user's, and
    // later user migrations chain off the user's head only, so the
    // framework tables fall out of the baseline. The diff then
    // re-emits `CreateTable` for them and the run fails with
    // `relation already exists`.
    //
    // So build the baseline in two extra steps:
    //   1. Fold in the snapshots of side-chain heads.
    //   2. Add every `rustango_*` table the registry knows. The
    //      framework owns that prefix, so an app migration must
    //      never create one of those tables.
    let mut prev_snapshot = prior_scoped
        .last()
        .map_or_else(empty_snapshot, |m| effective_snapshot(&prior_scoped, m));
    let in_chain = chain_membership(&prior_scoped);
    for m in &prior_scoped {
        if !in_chain.contains(m.name.as_str()) {
            fold_in_missing_tables(&mut prev_snapshot, &m.snapshot);
        }
    }
    fold_in_framework_tables(&mut prev_snapshot, current);
    let prev_snapshot = prev_snapshot.filtered_to_scope(model_scope);
    let mut current = current.clone();
    current.keep_unmanaged(&prev_snapshot);
    let current = &current;
    let prev_name = prior_scoped.last().map(|m| m.name.clone());
    // Numbering looks at the whole directory, so a tenant `0002` and
    // a registry `0002` cannot collide as filenames.
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

    warn_untracked_table_comments(current);
    let changes = detect_changes(&prev_snapshot, current);
    if changes.is_empty() {
        return Ok(None);
    }
    for w in super::diff::irreversible_drops(&prev_snapshot, &changes) {
        tracing::warn!(target: "rustango::migrate", "{w}");
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
        replaces: Vec::new(),
        merges: Vec::new(),
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

/// Snapshots do not carry `db_table_comment` yet, so file migrations
/// never write it; say so instead of dropping it silently.
fn warn_untracked_table_comments(current: &SchemaSnapshot) {
    for e in crate::core::inventory::iter::<crate::core::ModelEntry> {
        let Some(comment) = e.schema.db_table_comment else {
            continue;
        };
        if current.tables.iter().any(|t| t.name == e.schema.table) {
            tracing::warn!(
                target: "rustango::migrate",
                "`{}`: db_table_comment {comment:?} is not written by file migrations yet; \
                 add it with a data migration",
                e.schema.table,
            );
        }
    }
}

/// Generate the framework's own **system-app** migrations into
/// `<project_root>/system/migrations/`.
///
/// Diffs the framework's own `rustango_*` models for `scope` against
/// the prior system migrations of that scope.
///
/// Unlike [`make_migrations_scoped`], this does **not** fold
/// framework tables into the baseline: here they are the subject. A
/// fresh project gets `CreateTable` for each of them, and later runs
/// get `AddColumn` or `DropColumn` as the framework models change.
///
/// Returns `Ok(None)` when the framework's schema for this scope is
/// unchanged.
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
    let prior = if dir.exists() {
        file::list_dir(&dir)?
    } else {
        Vec::new()
    };
    let Some(mig) = plan_system_step(scope, name_override, &prior)? else {
        return Ok(None);
    };
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    let path = dir.join(format!("{}.json", mig.name));
    file::write(&path, &mig)?;
    Ok(Some(mig))
}

/// The step [`make_migrations_system`] would write after `prior`, unwritten,
/// so a `--dry-run` can show it.
///
/// # Errors
/// As [`make_migrations_system`], minus the write.
pub(crate) fn plan_system_step(
    scope: crate::core::ModelScope,
    name_override: Option<&str>,
    prior: &[Migration],
) -> Result<Option<Migration>, MigrateError> {
    // Skip the registry scope when no model declares it, which means
    // this build has no registry database. Its snapshot is still
    // non-empty, because the shared tables are copied into every
    // scope, so without this guard we write a registry `0001` that
    // creates tables the tenant migration also creates. Nothing
    // applies it until the project turns on tenancy, and then it
    // fails with `relation already exists`.
    //
    // Registry only, on purpose. In a single-database project the
    // tenant scope is the one that runs, and it carries the shared
    // tables, so skipping it would leave nothing to create them.
    if scope == crate::core::ModelScope::Registry
        && !super::snapshot::scope_owns_system_tables(scope)
    {
        return Ok(None);
    }
    let migration_scope = match scope {
        crate::core::ModelScope::Registry => super::MigrationScope::Registry,
        crate::core::ModelScope::Tenant => super::MigrationScope::Tenant,
    };
    super::snapshot::check_registry_junctions()?;
    let current = SchemaSnapshot::from_registry_system_for_scope(scope);
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
    Ok(Some(Migration {
        name,
        created_at,
        prev: prev_name,
        atomic: true,
        scope: migration_scope,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: current,
        forward: changes.into_iter().map(Operation::Schema).collect(),
    }))
}

/// The framework's `system/migrations/` dir and where its files came from.
///
/// Built only by [`SystemChain::prepare`], so a tenant cannot re-probe a dir
/// the registry or an earlier tenant filled and take it for shipped (#1988).
#[derive(Debug, Clone)]
pub(crate) struct SystemChain {
    dir: std::path::PathBuf,
    origin: super::runner::ChainOrigin,
}

impl SystemChain {
    /// [`Self::prepare`] for the project whose `migrations/` dir is `dir`.
    ///
    /// # Errors
    /// As [`Self::prepare`].
    pub(crate) fn for_migrations_dir(
        dir: &Path,
        scopes: &[crate::core::ModelScope],
    ) -> Result<Self, MigrateError> {
        Self::prepare(Self::project_root(dir), scopes)
    }

    /// The project root for a `migrations/` dir. A bare dir not named
    /// `migrations` keeps `system/` inside it.
    fn project_root(dir: &Path) -> &Path {
        if dir.file_name().and_then(|n| n.to_str()) == Some("migrations") {
            dir.parent().unwrap_or(dir)
        } else {
            dir
        }
    }

    /// The chain's dir for that project, as is: a preview reads it
    /// without generating anything.
    pub(crate) fn dir_for(dir: &Path) -> std::path::PathBuf {
        Self::project_root(dir).join("system").join("migrations")
    }

    /// Generate the chain for `scopes` from today's models. A dir that was
    /// empty when this process first saw it stays `Regenerated`: its names
    /// may already sit in a ledger for other content.
    ///
    /// # Errors
    /// A generation error, so `migrate` never applies a stale chain (#2014).
    pub(crate) fn prepare(
        project_root: &Path,
        scopes: &[crate::core::ModelScope],
    ) -> Result<Self, MigrateError> {
        use super::runner::ChainOrigin;
        static REGENERATED: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
        > = std::sync::OnceLock::new();
        let dir = project_root.join("system").join("migrations");
        // Held across probe and write, so a concurrent caller can't see half a dir.
        let mut regenerated = REGENERATED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shipped = file::list_dir(&dir).is_ok_and(|m| !m.is_empty());
        let generated = scopes
            .iter()
            .try_for_each(|&scope| make_migrations_system(project_root, scope, None).map(drop));
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let origin = if !shipped {
            regenerated.insert(key);
            ChainOrigin::Regenerated
        } else if regenerated.contains(&key) {
            ChainOrigin::Regenerated
        } else {
            ChainOrigin::OnDisk
        };
        // After the origin is recorded: a half-written dir is still regenerated.
        generated?;
        Ok(Self { dir, origin })
    }

    /// A shipped chain in `dir`, as is: no catch-up step from today's models.
    #[cfg(all(test, feature = "sqlite"))]
    pub(crate) fn shipped(dir: std::path::PathBuf) -> Self {
        Self {
            dir,
            origin: super::runner::ChainOrigin::OnDisk,
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(super) fn origin(&self) -> super::runner::ChainOrigin {
        self.origin
    }
}

/// [`make_migrations`] with the current snapshot passed in, instead
/// of read from the registry.
///
/// # Errors
/// Returns [`MigrateError::Io`] or [`MigrateError::Json`] on a file
/// problem, and [`MigrateError::Validation`] if a prior migration is
/// corrupt.
pub fn make_migrations_from(
    dir: &Path,
    current: &SchemaSnapshot,
    name_override: Option<&str>,
) -> Result<Option<Migration>, MigrateError> {
    let prior = file::list_dir(dir)?;
    let chain: Vec<&Migration> = prior.iter().collect();
    refuse_branched(&chain)?;
    let mut prev_snapshot = prior
        .last()
        .map_or_else(empty_snapshot, |m| effective_snapshot(&chain, m));
    // The framework owns its `rustango_*` tables: `migrate` creates
    // them from its own system migration chain on the first run. An
    // app diff must treat them as already present, or the next
    // `makemigrations` emits `CreateTable` for each one and the
    // following `migrate` fails on `already exists`.
    //
    // `make_migrations_system` does not fold: there those tables are
    // the subject.
    fold_in_framework_tables(&mut prev_snapshot, current);
    let mut current = current.clone();
    current.keep_unmanaged(&prev_snapshot);
    let current = &current;
    let prev_name = prior.last().map(|m| m.name.clone());
    let next_index = prior
        .last()
        .and_then(|m| extract_index(&m.name))
        .map_or(1, |n| n + 1);

    // Reject metadata changes no `SchemaChange` can express. Without
    // this the function would return `Ok(None)` and the user would
    // believe the schema was already up to date.
    let unsupported = detect_unsupported_field_changes(&prev_snapshot, current);
    if !unsupported.is_empty() {
        return Err(MigrateError::Validation(format!(
            "field metadata changed but v0.3 has no AlterField operation \
             (deferred to v0.4); the following changes need explicit migration \
             authoring:\n  - {}",
            unsupported.join("\n  - "),
        )));
    }

    warn_untracked_table_comments(current);
    let changes = detect_changes(&prev_snapshot, current);
    if changes.is_empty() {
        return Ok(None);
    }
    for w in super::diff::irreversible_drops(&prev_snapshot, &changes) {
        tracing::warn!(target: "rustango::migrate", "{w}");
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
        replaces: Vec::new(),
        merges: Vec::new(),
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

/// Names reachable from the last in-scope migration through every
/// parent (`prev` and merged branches). A name missing from this set
/// belongs to a side chain whose tables would otherwise drop out of the
/// baseline. See `make_migrations_scoped`.
fn chain_membership(prior_scoped: &[&Migration]) -> std::collections::HashSet<String> {
    let mut seen = std::collections::HashSet::new();
    let mut todo: Vec<&str> = prior_scoped
        .last()
        .map(|m| m.name.as_str())
        .into_iter()
        .collect();
    while let Some(name) = todo.pop() {
        // `insert` fails on a revisit, so a cyclic chain still ends.
        if !seen.insert(name.to_owned()) {
            continue;
        }
        if let Some(m) = prior_scoped.iter().find(|m| m.name == name) {
            todo.extend(m.prev.as_deref());
            todo.extend(merged_branches(prior_scoped, m));
        }
    }
    seen
}

/// The branch leaves `m` merges. A merge file written before `merges`
/// existed (0.60.5) joined every sibling of its `prev` named before it.
fn merged_branches<'a>(chain: &[&'a Migration], m: &'a Migration) -> Vec<&'a str> {
    if !m.merges.is_empty() {
        return m.merges.iter().map(String::as_str).collect();
    }
    let legacy = m.forward.is_empty() && m.name.ends_with("_merge") && m.replaces.is_empty();
    let Some(head) = m
        .prev
        .as_deref()
        .and_then(|p| chain.iter().find(|c| c.name == p))
        .filter(|_| legacy)
    else {
        return Vec::new();
    };
    chain
        .iter()
        .filter(|c| c.name != head.name && c.name < m.name && c.prev == head.prev)
        .map(|c| c.name.as_str())
        .collect()
}

/// `head`'s snapshot plus the renames hand-written into it and the copies
/// before it: `--empty` copies its parent's snapshot and the edit never
/// updates it (#2573). A snapshot that differs from its parent's already
/// holds its ops, so the walk stops there.
fn effective_snapshot(chain: &[&Migration], head: &Migration) -> SchemaSnapshot {
    let parent = |m: &Migration| {
        m.prev
            .as_deref()
            .and_then(|p| chain.iter().copied().find(|c| c.name == p))
    };
    let mut copies = vec![];
    let mut m = head;
    while let Some(p) = parent(m).filter(|p| p.snapshot == m.snapshot) {
        copies.push(m);
        if copies.len() > chain.len() {
            break; // a cyclic `prev`
        }
        m = p;
    }
    let mut snap = head.snapshot.clone();
    for op in copies.iter().rev().flat_map(|m| &m.forward) {
        if let Operation::Schema(change) = op {
            snap.apply_rename(change);
        }
    }
    snap
}

/// `m` and its `prev` ancestors, newest first.
fn ancestry<'a>(chain: &[&'a Migration], m: &'a Migration) -> Vec<&'a Migration> {
    let mut out = vec![m];
    while let Some(p) = out
        .last()
        .and_then(|c| c.prev.as_deref())
        .and_then(|p| chain.iter().copied().find(|c| c.name == p))
    {
        if out.iter().any(|o| o.name == p.name) {
            break; // a cyclic `prev`
        }
        out.push(p);
    }
    out
}

/// The schema rolling back `target` leaves: `prev` (its parent's snapshot)
/// plus the other branches still applied, which `prev` does not know. Falls
/// back to `prev` when those branches cannot be merged into it.
pub(super) fn rollback_snapshot(
    all: &[Migration],
    applied: &std::collections::HashSet<String>,
    target: &Migration,
    prev: SchemaSnapshot,
) -> SchemaSnapshot {
    let chain: Vec<&Migration> = all
        .iter()
        .filter(|m| m.name != target.name && applied.contains(&m.name))
        .collect();
    let Some(parent) = target
        .prev
        .as_deref()
        .and_then(|p| chain.iter().copied().find(|m| m.name == p))
    else {
        return prev;
    };
    with_other_branches(
        &chain,
        parent,
        Some(&target.name),
        prev,
        "unapplying",
        &target.name,
    )
}

/// The schema applying `mig` leaves: its own snapshot plus the other branches
/// already applied, so a SQLite rebuild keeps their columns.
pub(super) fn apply_snapshot(
    all: &[Migration],
    applied: &std::collections::HashSet<String>,
    mig: &Migration,
) -> SchemaSnapshot {
    let chain: Vec<&Migration> = all
        .iter()
        .filter(|m| m.name == mig.name || applied.contains(&m.name))
        .collect();
    let Some(tip) = chain.iter().copied().find(|m| m.name == mig.name) else {
        return mig.snapshot.clone();
    };
    with_other_branches(
        &chain,
        tip,
        None,
        mig.snapshot.clone(),
        "applying",
        &mig.name,
    )
}

/// `own` (the schema along `tip`'s line) merged with every other leaf of
/// `chain`, skipping leaves built on `skip`. Falls back to `own` on a conflict.
fn with_other_branches(
    chain: &[&Migration],
    tip: &Migration,
    skip: Option<&str>,
    own: SchemaSnapshot,
    verb: &str,
    name: &str,
) -> SchemaSnapshot {
    let line = ancestry(chain, tip);
    let mut snap = own.clone();
    for leaf in leaves(chain) {
        let reach = reachable(chain, leaf);
        if leaf.name == tip.name || skip.is_some_and(|s| reach.contains(s)) {
            continue;
        }
        let Some(base) = line.iter().find(|a| reach.contains(&a.name)) else {
            continue;
        };
        let theirs = effective_snapshot(chain, leaf);
        if let Err(conflicts) = snap.merge_branch(&effective_snapshot(chain, base), &theirs) {
            tracing::warn!(
                target: "rustango::migrate",
                "{verb} `{name}`: branch `{}` conflicts ({}); rebuilding from `{}` alone",
                leaf.name,
                conflicts.join("; "),
                tip.name,
            );
            return own;
        }
    }
    snap
}

/// Names reachable from `m` through `prev` and merged branches, `m`'s own
/// and names missing from `chain` included.
fn reachable(chain: &[&Migration], m: &Migration) -> std::collections::HashSet<String> {
    let mut seen = std::collections::HashSet::new();
    let mut todo = vec![m.name.as_str()];
    while let Some(name) = todo.pop() {
        if !seen.insert(name.to_owned()) {
            continue;
        }
        if let Some(c) = chain.iter().find(|c| c.name == name) {
            todo.extend(c.prev.as_deref());
            todo.extend(merged_branches(chain, c));
        }
    }
    seen
}

/// Migrations no `prev`, merged branch or `replaces` names.
fn leaves<'a>(chain: &[&'a Migration]) -> Vec<&'a Migration> {
    let joined: std::collections::HashSet<&str> = chain
        .iter()
        .flat_map(|m| {
            let named = m.prev.iter().chain(&m.replaces).map(String::as_str);
            named.chain(merged_branches(chain, m))
        })
        .collect();
    chain
        .iter()
        .copied()
        .filter(|m| !joined.contains(m.name.as_str()))
        .collect()
}

/// Leaves that branch from one history, grouped per root, each sorted by
/// name. Side chains with their own root are not branches (#2574).
fn branched_leaves<'a>(chain: &[&'a Migration]) -> Vec<Vec<&'a Migration>> {
    let mut groups: std::collections::BTreeMap<&str, Vec<&Migration>> = Default::default();
    for leaf in leaves(chain) {
        let root = ancestry(chain, leaf).last().map_or("", |r| r.name.as_str());
        groups.entry(root).or_default().push(leaf);
    }
    groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort_by(|a, b| a.name.cmp(&b.name));
            g
        })
        .collect()
}

/// Refuse to diff a branched chain: its last file holds one branch only.
fn refuse_branched(chain: &[&Migration]) -> Result<(), MigrateError> {
    let Some(branch) = branched_leaves(chain).into_iter().next() else {
        return Ok(());
    };
    let names: Vec<&str> = branch.iter().map(|m| m.name.as_str()).collect();
    Err(MigrateError::Validation(format!(
        "makemigrations: the chain has branched ({}); run `makemigrations --merge` first",
        names.join(", ")
    )))
}

/// Set every `rustango_*` table in `into` to its current shape. The
/// prefix is reserved: the system chain creates and changes those tables,
/// so an app diff must never emit a `CreateTable` or `AddColumn` for one.
fn fold_in_framework_tables(into: &mut SchemaSnapshot, current: &SchemaSnapshot) {
    let framework = |name: &str| name.starts_with("rustango_");
    into.tables.retain(|t| !framework(&t.name));
    into.tables.extend(
        current
            .tables
            .iter()
            .filter(|t| framework(&t.name))
            .cloned(),
    );
    into.m2m_tables.retain(|m| !framework(&m.through));
    into.m2m_tables.extend(
        current
            .m2m_tables
            .iter()
            .filter(|m| framework(&m.through))
            .cloned(),
    );
    into.indexes.retain(|i| !framework(&i.table));
    into.indexes.extend(
        current
            .indexes
            .iter()
            .filter(|i| framework(&i.table))
            .cloned(),
    );
    into.checks.retain(|c| !framework(&c.table));
    into.checks.extend(
        current
            .checks
            .iter()
            .filter(|c| framework(&c.table))
            .cloned(),
    );
}

/// Copy every table, m2m, index and check from `from` into `into`
/// that `into` does not already name. Used to fold a side chain's
/// snapshot into the baseline. Adding only what is missing keeps a
/// `DropTable` in the main chain from being undone.
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
        [SchemaChange::AlterColumnComment { table, column, .. }] => {
            format!("alter_comment_of_{column}_on_{table}")
        }
        [SchemaChange::AlterFkOnDelete { table, column, .. }] => {
            format!("alter_on_delete_of_{column}_on_{table}")
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
        [SchemaChange::DropIndex { name, .. }] => format!("drop_index_{name}"),
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
        // When every op just creates tables (plus their indexes),
        // name the file after those tables instead of `auto`. At
        // most 3 names, joined with `_and_`.
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

/// Reconcile a branched migration history. Backs
/// `makemigrations --merge`.
///
/// A *leaf* is a migration no other migration names as its `prev`. A
/// linear chain has one. Two people each running `makemigrations` on
/// their own branch create a second leaf with the same parent, and
/// after both merge the next `makemigrations` would pick one of them
/// at random as its `prev`.
///
/// This writes a new `NNNN_merge.json` with an empty `forward` whose
/// `prev` is the last leaf and whose `merges` names the others, so the
/// chain has one head again. Its snapshot joins the branches' snapshots,
/// not the models, so unmigrated edits still reach the next
/// `makemigrations`. An already-linear chain returns `Ok(None)`.
///
/// # Errors
/// Returns [`MigrateError::Validation`] when the chain cannot be
/// merged, or [`MigrateError::Io`] / [`MigrateError::Json`] on file
/// problems.
pub fn make_merge_migration(dir: &Path) -> Result<Option<Migration>, MigrateError> {
    let prior = file::list_dir(dir)?;
    if prior.is_empty() {
        return Err(MigrateError::Validation(
            "makemigrations --merge: no migrations in directory — nothing to merge".into(),
        ));
    }
    let chain: Vec<&Migration> = prior.iter().collect();
    let Some(branch) = branched_leaves(&chain).into_iter().next() else {
        let heads: Vec<&str> = leaves(&chain).iter().map(|m| m.name.as_str()).collect();
        if heads.len() < 2 {
            return Ok(None);
        }
        // Separate roots are side chains, not branches of one history.
        return Err(MigrateError::Validation(format!(
            "makemigrations --merge: leaves don't share a common parent — \
             can't auto-reconcile. Leaves: {}",
            heads.join(", ")
        )));
    };

    // The newest migration every leaf descends from.
    let paths: Vec<Vec<&Migration>> = branch.iter().map(|l| ancestry(&chain, l)).collect();
    let base = paths[0]
        .iter()
        .find(|a| {
            paths[1..]
                .iter()
                .all(|p| p.iter().any(|m| m.name == a.name))
        })
        .map_or_else(empty_snapshot, |a| effective_snapshot(&chain, a));
    let (head, others) = branch.split_last().expect("a branch has 2+ leaves");
    let mut snapshot = effective_snapshot(&chain, head);
    for leaf in others {
        if let Err(conflicts) = snapshot.merge_branch(&base, &effective_snapshot(&chain, leaf)) {
            return Err(MigrateError::Validation(format!(
                "makemigrations --merge: `{}` and `{}` both changed: {}. \
                 Reconcile the branches by hand, then merge again",
                head.name,
                leaf.name,
                conflicts.join("; "),
            )));
        }
    }

    // Point the merge node at the last leaf. Name order already applies
    // the other leaves first, and `forward` is empty, so running this
    // file does nothing.
    let merge_prev = head.name.clone();

    // Number from the highest index in the whole directory, not
    // just among the leaves.
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
        scope: head.scope,
        // A merge joins two branches, it does not collapse history.
        replaces: Vec::new(),
        merges: others.iter().map(|m| m.name.clone()).collect(),
        snapshot,
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
    // Diffs must split by `ModelScope`: registry models into a file
    // tagged `MigrationScope::Registry`, tenant models into one
    // tagged `MigrationScope::Tenant`. Mixing them puts a registry
    // ALTER in a tenant migration, where `search_path` resolves it
    // to the registry copy and the run fails.
    //
    // These tests drive the split helpers without touching the
    // global inventory.

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
            replaces: Vec::new(),
            merges: Vec::new(),
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
        // First call in an empty dir writes one file tagged
        // `MigrationScope::Tenant`.
        //
        // The table name must not start with `rustango_`: that
        // prefix is filtered out of an app diff baseline, so a
        // framework-shaped name would test nothing.
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
        // An old bootstrap snapshot holds both registry and tenant
        // framework tables. `filtered_to_scope` must drop the
        // registry ones before the diff, so a tenant-scope diff sees
        // only the new user table and emits nothing for
        // `rustango_operators`.
        //
        // A table missing from the inventory counts as Tenant, so
        // only tables declared `scope = Registry` are filtered out.
        let dir = tempdir();
        let prev = Migration {
            name: "0001_initial".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Tenant,
            replaces: Vec::new(),
            merges: Vec::new(),
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
        // With a registry `0001` and a tenant `0002` in the same
        // directory, the next tenant migration must be `0003`.
        // Numbering walks both scopes so filenames cannot collide.
        let dir = tempdir();
        let r = Migration {
            name: "0001_registry_initial".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Registry,
            replaces: Vec::new(),
            merges: Vec::new(),
            snapshot: snap_with(vec![t("rustango_orgs")]),
            forward: vec![],
        };
        let t1 = Migration {
            name: "0002_initial".into(),
            created_at: "2026-01-02T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: MigrationScope::Tenant,
            replaces: Vec::new(),
            merges: Vec::new(),
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

    // ============================================ framework-table fold
    //
    // The first `migrate` applies the system migration chain, which
    // creates every `rustango_*` table. So by the user's first
    // `makemigrations` those tables already exist while the app's
    // migration directory is still empty.
    //
    // An app diff baseline must therefore treat them as present.
    // Otherwise `makemigrations` emits `CreateTable` for each one and
    // the next `migrate` fails on `already exists`. These tests pin
    // that for the plain and `--app` paths, which both go through
    // `make_migrations_from`.

    fn idx(name: &str, table: &str) -> crate::migrate::snapshot::IndexSnapshot {
        crate::migrate::snapshot::IndexSnapshot {
            name: name.into(),
            table: table.into(),
            columns: vec!["id".into()],
            unique: false,
            method: "btree".into(),
            where_clause: None,
            include: vec![],
        }
    }

    /// A first `makemigrations` in an empty dir must create the
    /// user's tables and leave the framework's alone.
    #[test]
    fn plain_makemigrations_does_not_re_emit_framework_tables() {
        let dir = tempdir();
        // Two user models beside the framework tables the system
        // chain has already created.
        let current = snap_with(vec![
            t("blog"),
            t("item"),
            t("rustango_admin_users"),
            t("rustango_audit_log"),
            t("rustango_content_types"),
        ]);

        let mig = make_migrations_from(&dir, &current, None)
            .expect("diff succeeds")
            .expect("user tables are new, so a migration is written");

        let created: Vec<&str> = mig
            .forward
            .iter()
            .filter_map(|op| match op {
                Operation::Schema(SchemaChange::CreateTable(n)) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            created,
            vec!["blog", "item"],
            "a user-app migration must create only user tables; any \
             rustango_* table here is what crashes the next `migrate`"
        );
    }

    /// Indexes must be folded in too: re-creating an index on a
    /// framework table fails the same way re-creating the table does.
    #[test]
    fn plain_makemigrations_does_not_re_emit_framework_indexes() {
        let dir = tempdir();
        let mut current = snap_with(vec![t("blog"), t("rustango_audit_log")]);
        current.indexes = vec![
            idx("blog_id_idx", "blog"),
            idx("rustango_audit_log_occurred_at_idx", "rustango_audit_log"),
        ];

        let mig = make_migrations_from(&dir, &current, None)
            .expect("diff succeeds")
            .expect("migration written");

        let indexed: Vec<&str> = mig
            .forward
            .iter()
            .filter_map(|op| match op {
                Operation::Schema(SchemaChange::CreateIndex { table, .. }) => Some(table.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            indexed,
            vec!["blog"],
            "framework indexes belong to the system chain"
        );
    }

    /// With only framework tables present there is nothing for an
    /// app migration to write.
    #[test]
    fn framework_only_registry_yields_no_migration() {
        let dir = tempdir();
        let current = snap_with(vec![t("rustango_admin_users"), t("rustango_media")]);
        assert!(
            make_migrations_from(&dir, &current, None)
                .expect("diff succeeds")
                .is_none(),
            "nothing but framework tables means no user-app migration"
        );
    }

    /// The fold matches the reserved prefix only. A user table whose
    /// name merely contains `rustango` still belongs to the user.
    #[test]
    fn fold_only_matches_the_reserved_prefix() {
        let dir = tempdir();
        let current = snap_with(vec![t("my_rustango_notes"), t("rustango_media")]);

        let mig = make_migrations_from(&dir, &current, None)
            .expect("diff succeeds")
            .expect("the user table is new");

        let created: Vec<&str> = mig
            .forward
            .iter()
            .filter_map(|op| match op {
                Operation::Schema(SchemaChange::CreateTable(n)) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(created, vec!["my_rustango_notes"]);
    }

    // ====================================== makemigrations --merge
    //
    // Reconciles a branched chain with an empty-forward
    // `NNNN_merge.json` whose `prev` is the last leaf. Its snapshot
    // joins the leaves' snapshots.

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
            replaces: Vec::new(),
            merges: Vec::new(),
            snapshot,
            forward: vec![],
        }
    }

    /// Two `--empty` renames in a row: both reach the next diff (#2573).
    #[test]
    fn stacked_empty_renames_reach_the_baseline() {
        let dir = tempdir();
        let rename = |table: &str, old: &str, new: &str| {
            Operation::Schema(SchemaChange::RenameColumn {
                table: table.into(),
                old_column: old.into(),
                new_column: new.into(),
            })
        };
        let with = |cols: &[&str]| snap_with(vec![posts_with(cols)]);
        let old = with(&["a", "c"]);
        write_mig(&dir, &mig_at("0001_initial", None, old.clone()));
        let mut m2 = mig_at("0002_r1", Some("0001_initial"), old.clone());
        m2.forward = vec![rename("posts", "a", "b")];
        write_mig(&dir, &m2);
        let mut m3 = mig_at("0003_r2", Some("0002_r1"), old);
        m3.forward = vec![rename("posts", "c", "d")];
        write_mig(&dir, &m3);
        let next = make_migrations_from(&dir, &with(&["b", "d"]), None).unwrap();
        assert!(next.is_none(), "{:?}", next.map(|m| m.forward));
        // `makemigrations` itself takes the scoped path.
        let next = make_migrations_scoped(
            &dir,
            &with(&["b", "d"]),
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap();
        assert!(next.is_none(), "{:?}", next.map(|m| m.forward));
        let _ = std::fs::remove_dir_all(&dir);
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
        let r = make_merge_migration(&dir).unwrap();
        assert!(
            r.is_none(),
            "single-leaf chain should NOT emit a merge file, got {r:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_errors_when_dir_is_empty() {
        let dir = tempdir();
        let err = make_merge_migration(&dir).unwrap_err();
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

        let mig = make_merge_migration(&dir)
            .unwrap()
            .expect("two leaves should produce a merge file");

        // Next index = max(0001, 0002a, 0002b) = 2 → next is 0003.
        assert!(
            mig.name.starts_with("0003_"),
            "merge file index should follow lex-last leaf, got: {}",
            mig.name
        );
        assert_eq!(mig.name, "0003_merge");
        // The last leaf by name wins `prev`: 0002b sorts after 0002a.
        assert_eq!(mig.prev.as_deref(), Some("0002b_branch"));
        // The merge file only anchors the chain, so it runs nothing.
        assert!(mig.forward.is_empty(), "merge file must have empty forward");
        // Name order still applies 0002a, then 0002b, then the merge
        // file, so both branches' changes land. 0002a stays a leaf
        // afterwards because nothing names it, but that is a graph
        // quirk, not a chain conflict, so the test stops here.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_rejects_leaves_with_different_parents() {
        let dir = tempdir();
        let snap = snap_with(vec![t("posts")]);
        // Two leaves with different parents is a real divergence,
        // not a branch collision, so the merge must refuse.
        write_mig(&dir, &mig_at("0001_a", None, snap.clone()));
        write_mig(&dir, &mig_at("0002_b", Some("0001_a"), snap.clone()));
        write_mig(&dir, &mig_at("0001_z", None, snap.clone()));
        let err = make_merge_migration(&dir).unwrap_err();
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

    /// `posts` with `cols` beside its `id`.
    fn posts_with(cols: &[&str]) -> TableSnapshot {
        let mut table = t("posts");
        for c in cols {
            let mut f = table.fields[0].clone();
            (f.name, f.column, f.primary_key, f.auto) = ((*c).into(), (*c).into(), false, false);
            table.fields.push(f);
        }
        table.fields.sort_by(|a, b| a.column.cmp(&b.column));
        table
    }

    /// 0001 ← 0002a (adds `a`, drops `old`) and 0001 ← 0002b (adds `b`
    /// and the `tags` table).
    fn branched_dir() -> std::path::PathBuf {
        let dir = tempdir();
        let base = snap_with(vec![posts_with(&["old"])]);
        write_mig(&dir, &mig_at("0001_initial", None, base));
        let a = snap_with(vec![posts_with(&["a"])]);
        write_mig(&dir, &mig_at("0002a", Some("0001_initial"), a));
        let b = snap_with(vec![posts_with(&["b", "old"]), t("tags")]);
        write_mig(&dir, &mig_at("0002b", Some("0001_initial"), b));
        dir
    }

    /// A merged-in branch is part of the chain, not a side chain whose
    /// tables and indexes fold back into the baseline.
    #[test]
    fn scoped_makemigrations_follows_merges() {
        let dir = tempdir();
        let mut base = snap_with(vec![posts_with(&[])]);
        base.indexes = vec![idx("posts_x_idx", "posts")];
        write_mig(&dir, &mig_at("0001_initial", None, base));
        let mut a = snap_with(vec![posts_with(&["a"])]);
        a.indexes = vec![idx("posts_x_idx", "posts")];
        write_mig(&dir, &mig_at("0002a", Some("0001_initial"), a));
        let b = snap_with(vec![posts_with(&[])]);
        write_mig(&dir, &mig_at("0002b", Some("0001_initial"), b));
        let mig = make_merge_migration(&dir).unwrap().unwrap();
        assert!(mig.snapshot.indexes.is_empty(), "{:?}", mig.snapshot);
        let next = make_migrations_scoped(
            &dir,
            &mig.snapshot,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap();
        assert!(next.is_none(), "{:?}", next.map(|m| m.forward));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A merge file from before `merges` existed still joins its branch.
    #[test]
    fn legacy_merge_file_still_joins_the_branch() {
        let dir = branched_dir();
        let joined = snap_with(vec![posts_with(&["a", "b"]), t("tags")]);
        write_mig(&dir, &mig_at("0003_merge", Some("0002b"), joined.clone()));
        assert!(make_migrations_from(&dir, &joined, None).unwrap().is_none());
        let scoped = make_migrations_scoped(
            &dir,
            &joined,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap();
        assert!(scoped.is_none(), "{:?}", scoped.map(|m| m.forward));
        assert!(make_merge_migration(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Diffing one branch would re-add the other's column (#2574).
    #[test]
    fn makemigrations_refuses_a_branched_chain() {
        let dir = branched_dir();
        let current = snap_with(vec![posts_with(&["a", "b"]), t("tags")]);
        let err = make_migrations_from(&dir, &current, None).unwrap_err();
        assert!(
            matches!(&err, MigrateError::Validation(m) if m.contains("--merge")),
            "{err:?}"
        );
        let err = make_migrations_scoped(
            &dir,
            &current,
            ModelScope::Tenant,
            MigrationScope::Tenant,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, MigrateError::Validation(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The merge snapshot joins both branches and keeps an unmigrated
    /// model edit out, so the next diff still emits it (#2574).
    #[test]
    fn merge_joins_the_branches_not_the_models() {
        let dir = branched_dir();
        let mig = make_merge_migration(&dir).unwrap().unwrap();
        assert_eq!(mig.merges, vec!["0002a".to_owned()]);
        let joined = snap_with(vec![posts_with(&["a", "b"]), t("tags")]);
        assert_eq!(mig.snapshot, joined);
        let mut current = joined;
        current.tables[0] = posts_with(&["a", "b", "c"]);
        let next = make_migrations_from(&dir, &current, None)
            .unwrap()
            .expect("the unmigrated column");
        assert_eq!(next.prev.as_deref(), Some(mig.name.as_str()));
        assert_eq!(
            next.forward,
            vec![Operation::Schema(SchemaChange::AddColumn {
                table: "posts".into(),
                column: "c".into(),
            })]
        );
        assert!(make_merge_migration(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both branches changing one thing differently is refused, not won by
    /// the other branch or the drop.
    #[test]
    fn merge_refuses_conflicting_branches() {
        let mut nullable = posts_with(&["c"]);
        nullable
            .fields
            .iter_mut()
            .find(|f| f.column == "c")
            .unwrap()
            .nullable = true;
        let mut longer = posts_with(&["c"]);
        longer
            .fields
            .iter_mut()
            .find(|f| f.column == "c")
            .unwrap()
            .max_length = Some(9);
        // Both add `tags.x`, with different types.
        let tags = |ty: &str| {
            let mut t = t("tags");
            let mut x = t.fields[0].clone();
            (x.name, x.column, x.primary_key, x.auto, x.ty) =
                ("x".into(), "x".into(), false, false, ty.into());
            t.fields.push(x);
            t
        };
        let model = |m: &str| TableSnapshot {
            model: m.into(),
            ..posts_with(&["c"])
        };
        for (a, b, what) in [
            (vec![nullable.clone()], vec![], "table `posts`"),
            (vec![], vec![nullable.clone()], "table `posts`"),
            (vec![nullable], vec![longer], "column `c`"),
            (
                vec![posts_with(&["c"]), tags("i64")],
                vec![posts_with(&["c"]), tags("string")],
                "table `tags` column `x`",
            ),
            (
                vec![model("PostA")],
                vec![model("PostB")],
                "table `posts` model",
            ),
        ] {
            let dir = tempdir();
            write_mig(
                &dir,
                &mig_at("0001_initial", None, snap_with(vec![posts_with(&["c"])])),
            );
            write_mig(&dir, &mig_at("0002a", Some("0001_initial"), snap_with(a)));
            write_mig(&dir, &mig_at("0002b", Some("0001_initial"), snap_with(b)));
            let err = make_merge_migration(&dir).expect_err(what);
            assert!(
                matches!(&err, MigrateError::Validation(m) if m.contains(what)),
                "{err:?}"
            );
            assert!(!dir.join("0003_merge.json").exists(), "{what}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
