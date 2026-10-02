//! On-disk migration file format.
//!
//! One JSON file per migration, named so a lexical sort gives the
//! apply order (`0001_initial.json`, `0002_add_bio_to_author.json`).
//! Each file holds the **full schema snapshot** at that point, so it
//! can stand alone as a starting state and a rollback can recover the
//! metadata of a dropped field without reading its predecessors.
//!
//! `forward` is one ordered list of [`Operation`]s, mixing schema and
//! data steps. That lets one migration do the usual "add a nullable
//! column, backfill it, then set NOT NULL".
//!
//! ```json
//! {
//!   "name": "0002_backfill_slugs",
//!   "created_at": "2026-04-28T10:00:00Z",
//!   "prev": "0001_initial",
//!   "atomic": true,
//!   "snapshot": { "tables": [/* SchemaSnapshot */] },
//!   "forward": [
//!     { "schema": { "AddColumn": { "table": "article", "column": "slug" } } },
//!     { "data":   { "sql": "UPDATE article SET slug = ...",
//!                   "reverse_sql": "UPDATE article SET slug = NULL",
//!                   "reversible": true } }
//!   ]
//! }
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::diff::SchemaChange;
use super::error::MigrateError;
use super::snapshot::SchemaSnapshot;

/// One migration on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    /// Lex-sortable file stem, e.g. `0002_add_bio_to_author`. Apply order.
    pub name: String,
    /// RFC3339 timestamp written by `make_migrations`. For humans
    /// only; the runner ignores it.
    pub created_at: String,
    /// Predecessor migration name. `None` for `0001_initial`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prev: Option<String>,
    /// Wrap the migration in a transaction. Default `true`. Set
    /// `false` for statements that cannot run in one, such as
    /// `CREATE INDEX CONCURRENTLY`.
    #[serde(default = "default_atomic")]
    pub atomic: bool,
    /// Whether this runs against the registry or every tenant.
    /// Defaults to `Tenant`, which covers most schema work.
    #[serde(default, skip_serializing_if = "MigrationScope::is_default")]
    pub scope: MigrationScope,
    /// The migrations this one **replaces**, for a squash.
    ///
    /// A squash folds a run of old migrations into one file that
    /// reaches the same end state. On a fresh database it just runs.
    /// On a database that already applied the replaced migrations the
    /// runner reconciles instead: it records the squash and tombstones
    /// the old ledger rows, running no DDL. See
    /// [`crate::migrate::migrate_pool`] and the `--fake` flag.
    ///
    /// Empty for an ordinary migration, and then left out of the JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
    /// Full schema snapshot **after** `forward` has been applied.
    pub snapshot: SchemaSnapshot,
    /// Ordered operations, schema and data mixed.
    pub forward: Vec<Operation>,
}

/// Where a migration runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MigrationScope {
    /// Runs once against the registry database. For registry-only
    /// tables such as `rustango_orgs` or `rustango_operators`.
    Registry,
    /// Runs against every active org's storage. The default, and
    /// almost all user schema work.
    #[default]
    Tenant,
}

impl MigrationScope {
    /// Used by serde's `skip_serializing_if` to keep the default
    /// (`Tenant`) out of migration files.
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Tenant)
    }
}

fn default_atomic() -> bool {
    true
}

/// One step inside [`Migration::forward`].
///
/// Externally tagged with lowercase variant names so the JSON reads as
/// `{"schema": …}` / `{"data": …}` / `{"callback": …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// A schema change, in the same shape as the diff IR
    /// ([`SchemaChange`]). Rendered by `render_changes` at apply time.
    Schema(SchemaChange),
    /// Raw SQL the user wrote by hand, usually a backfill.
    Data(DataOp),
    /// Calls a named Rust callback at apply time. Register it with
    /// [`crate::register_migration_callback!`] at startup.
    Callback(CallbackOp),
}

/// Names the Rust callback an [`Operation::Callback`] runs. The runner
/// looks the name up in [`crate::migrate::callbacks`] at apply time;
/// an unknown name is a [`crate::migrate::MigrateError::Validation`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallbackOp {
    /// Forward-callback name. Required.
    pub name: String,
    /// Reverse-callback name. With `None`, rollback fails rather than
    /// skipping the step.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reverse_name: Option<String>,
}

/// User-written SQL plus its inverse. Both go into the statement
/// as-is: no escaping and no parameter binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataOp {
    /// SQL run when the migration is applied forward.
    pub sql: String,
    /// SQL run on rollback. `None` is valid only when `reversible` is
    /// `false`; loading rejects the contradiction.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reverse_sql: Option<String>,
    /// `false` marks the op one-way. Rollback then fails instead of
    /// quietly skipping it.
    #[serde(default = "default_reversible")]
    pub reversible: bool,
}

fn default_reversible() -> bool {
    true
}

/// Read and parse a migration file.
///
/// # Errors
/// Returns [`MigrateError::Io`] if the file is unreadable, or
/// [`MigrateError::Json`] if its contents don't deserialize. Also
/// rejects an internally-inconsistent `Operation::Data` where
/// `reversible == true` but `reverse_sql` is missing, and a callback in
/// an atomic migration (#1626).
pub fn load(path: &Path) -> Result<Migration, MigrateError> {
    let raw = std::fs::read_to_string(path)?;
    parse(&raw)
}

/// Parse and validate a migration from a JSON string. Used by
/// [`load`], and by `migrate_embedded` for `include_str!` bytes.
///
/// # Errors
/// Returns [`MigrateError::Json`] on parse failure or
/// [`MigrateError::Validation`] on internal-consistency failures.
pub fn parse(raw: &str) -> Result<Migration, MigrateError> {
    let mig: Migration = serde_json::from_str(raw)?;
    validate(&mig)?;
    Ok(mig)
}

/// Serialize and write a migration file (pretty-printed).
///
/// # Errors
/// Returns [`MigrateError::Io`] on write failure or
/// [`MigrateError::Json`] on serialization failure.
pub fn write(path: &Path, migration: &Migration) -> Result<(), MigrateError> {
    let raw = serde_json::to_string_pretty(migration)?;
    std::fs::write(path, raw)?;
    Ok(())
}

/// Load every `*.json` migration in `dir`, sorted lexicographically by
/// file name (which is the canonical apply order).
///
/// A missing `dir` gives an empty list, which is what a fresh project
/// needs. Each file is validated by [`load`], and the `prev` chain by
/// [`validate_chain`], so a migration naming a predecessor that is not
/// there fails here with a clear error instead of deep inside
/// `unapply` or `migrate_to`.
///
/// # Errors
/// Returns [`MigrateError::Io`] on read failure, [`MigrateError::Json`]
/// on parse failure, or [`MigrateError::Validation`] if any file is
/// internally inconsistent or the chain is broken.
pub fn list_dir(dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        out.push(load(&p)?);
    }
    validate_chain(&out, &dir.display().to_string())?;
    Ok(out)
}

/// [`list_dir`] over several directories. Loads each one, then sorts
/// the merged list by `name`, so apply order does not depend on which
/// app contributed which file.
///
/// Each directory's `prev` chain is checked on its own. Chains are not
/// required to link across directories: two independent apps have no
/// reason to chain through each other.
///
/// # Errors
/// Whatever [`list_dir`] returns for any of the inputs.
pub fn list_dirs<I, P>(dirs: I) -> Result<Vec<Migration>, MigrateError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut out: Vec<Migration> = Vec::new();
    for dir in dirs {
        out.extend(list_dir(dir.as_ref())?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Every migrations directory under `project_root`: the top-level
/// `migrations/`, plus one per app at `<app>/migrations/`.
///
/// Used by `Builder::migrate(project_root)` so a multi-app project
/// migrates in one call. Returns paths rather than loaded migrations,
/// so a caller can filter by scope first.
///
/// The top-level directory comes first, so its bootstraps run before
/// app content; app directories follow in name order.
#[must_use]
pub fn discover_migration_dirs(project_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let flat = project_root.join("migrations");
    if flat.is_dir() {
        out.push(flat);
    }
    if let Ok(read) = std::fs::read_dir(project_root) {
        let mut app_dirs: Vec<PathBuf> = read
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() {
                    return None;
                }
                // Skip the top-level migrations dir, already added,
                // and folders that are clearly not apps.
                let name = path.file_name()?.to_str()?;
                // `system` is the framework's own app. Tenancy
                // provisioning applies its migrations by scope under
                // a separate ledger, so it is not a plain app dir.
                if matches!(
                    name,
                    "migrations" | "target" | "src" | ".git" | "node_modules" | "system"
                ) || name.starts_with('.')
                {
                    return None;
                }
                let candidate = path.join("migrations");
                if candidate.is_dir() {
                    Some(candidate)
                } else {
                    None
                }
            })
            .collect();
        app_dirs.sort();
        out.extend(app_dirs);
    }
    out
}

/// Check that every `prev` names another migration in the same list.
/// `origin` is a directory path or a label, and appears in the error
/// so the reader knows where the migrations came from.
///
/// # Errors
/// Returns [`MigrateError::Validation`] on the first broken link.
pub(crate) fn validate_chain(migrations: &[Migration], origin: &str) -> Result<(), MigrateError> {
    for mig in migrations {
        if let Some(prev) = &mig.prev {
            if !migrations.iter().any(|m| &m.name == prev) {
                return Err(MigrateError::Validation(format!(
                    "broken migration chain: `{}` declares prev=`{prev}` but that migration is missing from {origin}",
                    mig.name,
                )));
            }
        }
    }
    Ok(())
}

/// Extract the leading numeric prefix from a migration name
/// (e.g. `0042_add_slug` → `42`).
#[must_use]
pub fn extract_index(name: &str) -> Option<u32> {
    let prefix: String = name.chars().take_while(char::is_ascii_digit).collect();
    if prefix.is_empty() {
        None
    } else {
        prefix.parse().ok()
    }
}

fn validate(mig: &Migration) -> Result<(), MigrateError> {
    for (i, op) in mig.forward.iter().enumerate() {
        match op {
            Operation::Data(d) => {
                if d.reversible && d.reverse_sql.is_none() {
                    return Err(MigrateError::Validation(format!(
                        "{}: forward[{}]: reversible=true but reverse_sql is missing",
                        mig.name, i,
                    )));
                }
            }
            // The callback runs on a second connection; inside the tx it
            // waits on the tx's locks, forever on PostgreSQL (#1626).
            Operation::Callback(c) if mig.atomic => {
                return Err(MigrateError::Validation(format!(
                    "{}: forward[{}]: callback `{}` needs `\"atomic\": false` \
                     (#1626); safe to add to an already-applied file",
                    mig.name, i, c.name,
                )));
            }
            _ => {}
        }
    }
    Ok(())
}
