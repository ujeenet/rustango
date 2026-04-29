//! On-disk migration file format.
//!
//! One JSON file per migration, lex-sortable by `name`
//! (e.g. `0001_initial.json`, `0002_add_bio_to_author.json`). Each file
//! stores the **full schema snapshot** at that point — so any one file
//! can stand on its own as a starting state, and future inverse-diffs
//! have access to dropped fields' metadata without chasing predecessors.
//!
//! `forward` is a flat ordered list of [`Operation`]s — `Schema` and
//! `Data` interleaved so callers can write the canonical
//! "add nullable → backfill via SQL → set NOT NULL" recipe in one
//! migration. Slice 3 of v0.3 will execute them in order; Slice 4 will
//! invert this list to roll back.
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

use crate::diff::SchemaChange;
use crate::error::MigrateError;
use crate::snapshot::SchemaSnapshot;

/// One migration on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    /// Lex-sortable file stem, e.g. `0002_add_bio_to_author`. Apply order.
    pub name: String,
    /// RFC3339 timestamp set by `make_migrations` when the file is written.
    /// Informational only — the apply runner ignores it.
    pub created_at: String,
    /// Predecessor migration name. `None` for `0001_initial`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prev: Option<String>,
    /// Wrap the migration in a transaction. Default `true`. Set `false`
    /// for migrations that can't run inside a tx (e.g. `CREATE INDEX
    /// CONCURRENTLY`).
    #[serde(default = "default_atomic")]
    pub atomic: bool,
    /// Where this migration runs — registry vs tenant. Default
    /// `Tenant` (most schema work is tenant-shaped). v0.5+ scoped
    /// migrations use this to route between `migrate_registry` (runs
    /// once against the registry DB) and `migrate_tenants` (fans out
    /// across active orgs). Pre-v0.5 migrations missing the field
    /// deserialize as `Tenant` and the runner ignores the distinction
    /// until v0.5 Slice 3 wires it in.
    #[serde(default, skip_serializing_if = "MigrationScope::is_default")]
    pub scope: MigrationScope,
    /// Full schema snapshot **after** applying `forward`.
    pub snapshot: SchemaSnapshot,
    /// Ordered list of operations — schema and data interleaved.
    pub forward: Vec<Operation>,
}

/// Where a migration runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MigrationScope {
    /// Cross-tenant — runs once against the registry DB. Reserved
    /// for migrations that touch `rustango_orgs`,
    /// `rustango_operators`, audit logs, or any other registry-only
    /// table.
    Registry,
    /// Per-tenant — runs against every active org's storage (schema
    /// or dedicated DB). Default; covers ~all user schema work.
    #[default]
    Tenant,
}

impl MigrationScope {
    /// Used by serde's `skip_serializing_if` so the default
    /// (`Tenant`) doesn't clutter migration files.
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
/// `{"schema": …}` / `{"data": …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// A schema change — same shape as the diff IR
    /// ([`SchemaChange`]). Routed through `render_changes` at apply time.
    Schema(SchemaChange),
    /// Raw SQL the user wrote by hand, typically a backfill.
    Data(DataOp),
}

/// User-authored SQL plus its inverse. Both fields are raw Postgres
/// expressions inserted verbatim — no escaping, no parameter binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataOp {
    /// SQL run when the migration is applied forward.
    pub sql: String,
    /// SQL run when the migration is rolled back. `None` is only valid
    /// when `reversible == false`; the load step rejects the contradiction.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reverse_sql: Option<String>,
    /// Mark a migration as one-way. Rollback fails fast on irreversible
    /// ops rather than silently no-op'ing — Django's footgun avoided.
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
/// `reversible == true` but `reverse_sql` is missing.
pub fn load(path: &Path) -> Result<Migration, MigrateError> {
    let raw = std::fs::read_to_string(path)?;
    parse(&raw)
}

/// Parse + validate a migration from an in-memory JSON string. Used
/// by [`load`] (after `read_to_string`) and by `migrate_embedded`
/// (which gets the bytes via `include_str!`).
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
/// [`MigrateError::Json`] on serialization failure (extremely unlikely
/// for our well-typed schema).
pub fn write(path: &Path, migration: &Migration) -> Result<(), MigrateError> {
    let raw = serde_json::to_string_pretty(migration)?;
    std::fs::write(path, raw)?;
    Ok(())
}

/// Load every `*.json` migration in `dir`, sorted lexicographically by
/// file name (which is the canonical apply order).
///
/// A non-existent `dir` is treated as an empty list — useful for
/// `make_migrations` against a fresh project. Each file is fully
/// validated via [`load`], and the cross-file `prev` chain is
/// validated via [`validate_chain`] — a migration declaring
/// `prev: "0002_missing"` whose predecessor isn't present fails at
/// load time with a clear "broken migration chain" error rather than
/// surfacing as a confusing failure deep inside `unapply` or
/// `migrate_to`.
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

/// Verify every migration's `prev` reference points to another
/// migration in the slice. Caller passes `origin` (a dir path or a
/// label like `"embedded slice"`) so the error message names where
/// the migrations came from.
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
        if let Operation::Data(d) = op {
            if d.reversible && d.reverse_sql.is_none() {
                return Err(MigrateError::Validation(format!(
                    "{}: forward[{}]: reversible=true but reverse_sql is missing",
                    mig.name, i,
                )));
            }
        }
    }
    Ok(())
}
