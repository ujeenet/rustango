//! `adopt`, the offline part (9.2): one checkpoint per app folder from the
//! last engine v1 snapshot, then the v1 files go (git keeps them).
//!
//! The state is written for one backend: defaults as engine v1 rendered
//! them there, and the names v1 left to that backend where it has them.
//! Names a backend never stores (an unnamed SQLite constraint) get v1's
//! PG spelling; the drift check matches those by shape.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::render::dialect;
use super::state::{
    pg_object_name, Backend, CheckDef, ColumnDef, ExternColumn, ExternOwner, ExternTable, FkDef,
    HashMemo, IndexDef, OnDelete, State, StateDoc, TableState, Target,
};
use super::{Code, LoadError};
use crate::migrate::engine::ProjectEngine;
use crate::migrate::file::{self, Migration, MigrationScope, Operation};
use crate::migrate::snapshot::{FieldSnapshot, SchemaSnapshot, TableSnapshot};

/// The prefix of the framework's own tables: external to project chains.
const FRAMEWORK_PREFIX: &str = "rustango_";

/// What `adopt` did.
#[derive(Debug, Default)]
pub(crate) struct AdoptReport {
    pub(crate) written: Vec<PathBuf>,
    pub(crate) removed: Vec<PathBuf>,
    /// v1 data ops and callbacks: they do not run on fresh installs after the switch.
    pub(crate) data_steps: Vec<String>,
}

fn err(code: Code, at: impl Into<String>, msg: impl Into<String>) -> LoadError {
    LoadError::new(code, at, msg)
}

/// Switch the engine v1 project around `dir` to engine v2 for backend `b`.
///
/// # Errors
/// Not a v1 project, a tenancy folder (phase 5), two leaves, an op v2 does
/// not run yet (composite FKs, M2M, exclusions), or a checkpoint file that
/// exists already.
pub(crate) fn adopt(dir: &Path, b: Backend) -> Result<AdoptReport, LoadError> {
    let at = dir.display().to_string();
    let engine =
        ProjectEngine::of_project(dir).map_err(|e| err(Code::Engine, &at, e.to_string()))?;
    if engine != ProjectEngine::V1 {
        return Err(err(
            Code::Engine,
            &at,
            "only an engine v1 project can be adopted",
        ));
    }
    let mut planned: Vec<(PathBuf, Vec<PathBuf>, serde_json::Value)> = Vec::new();
    let mut report = AdoptReport::default();
    for folder in crate::migrate::file::project_migration_dirs(dir) {
        let fat = folder.display().to_string();
        let migs = file::list_dir(&folder).map_err(|e| err(Code::File, &fat, e.to_string()))?;
        let Some(last) = migs.last() else { continue };
        if migs.iter().any(|m| m.scope == MigrationScope::Registry) {
            return Err(err(
                Code::NotEnabled,
                &fat,
                "a tenancy folder (registry migrations) is adopted from a later release",
            ));
        }
        let prior: Vec<&Migration> = migs.iter().collect();
        crate::migrate::make::refuse_branched(&prior)
            .map_err(|e| err(Code::Graph, &fat, e.to_string()))?;
        let snapshot = crate::migrate::make::legacy_baseline(
            &prior,
            &SchemaSnapshot::default(),
            crate::core::ModelScope::Tenant,
        );
        let doc = state_of(&snapshot, b, &fat)?;
        let hash = State::from_doc(doc.clone()).hash(&mut HashMemo::default());
        let next = file::extract_index(&last.name).map_or(1, |n| n + 1);
        let id = format!("{next:04}_adopt");
        let app = app_label(crate::migrate::file::project_root_of(dir), &folder);
        let body = json!({
            "engine": 2, "format": 1, "kind": "checkpoint", "id": id, "app": app,
            "target": "tenant", "parents": [],
            "base": {"legacy": last.name, "state_sha256": hash},
            "state": doc,
            "operations": [],
            "description": "Switched from engine v1. Rollback stops here.",
        });
        for m in &migs {
            for op in &m.forward {
                match op {
                    Operation::Data(_) => report.data_steps.push(format!("{}: data op", m.name)),
                    Operation::Callback(c) => {
                        report
                            .data_steps
                            .push(format!("{}: callback `{}`", m.name, c.name));
                    }
                    Operation::Schema(_) => {}
                }
            }
        }
        let v1_files: Vec<PathBuf> = migs
            .iter()
            .map(|m| folder.join(format!("{}.json", m.name)))
            .collect();
        planned.push((folder.join(format!("{id}.json")), v1_files, body));
    }
    if planned.is_empty() {
        return Err(err(Code::Engine, &at, "no engine v1 migrations to adopt"));
    }
    // Every checkpoint is written before any v1 file goes.
    for (path, _, body) in &planned {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| err(Code::Io, path.display().to_string(), e.to_string()))?;
        let text = serde_json::to_string_pretty(body).expect("json");
        f.write_all(text.as_bytes())
            .map_err(|e| err(Code::Io, path.display().to_string(), e.to_string()))?;
        report.written.push(path.clone());
    }
    for (_, v1, _) in planned {
        for p in v1 {
            std::fs::remove_file(&p)
                .map_err(|e| err(Code::Io, p.display().to_string(), e.to_string()))?;
            report.removed.push(p);
        }
    }
    Ok(report)
}

/// `project` for the flat `migrations/` (or a bare folder), else the app
/// folder's name (`<root>/<app>/migrations`).
fn app_label(root: &Path, folder: &Path) -> String {
    match folder.parent() {
        Some(parent) if parent != root && folder.file_name().is_some_and(|n| n == "migrations") => {
            parent
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_owned()
        }
        _ => "project".to_owned(),
    }
}

fn on_delete(raw: Option<&str>, at: &str) -> Result<OnDelete, LoadError> {
    Ok(match raw.map(str::to_ascii_uppercase).as_deref() {
        None | Some("NO ACTION" | "DO_NOTHING" | "DO NOTHING") => OnDelete::NoAction,
        Some("RESTRICT" | "PROTECT") => OnDelete::Restrict,
        Some("CASCADE") => OnDelete::Cascade,
        Some("SET NULL" | "SET_NULL") => OnDelete::SetNull,
        Some("SET DEFAULT" | "SET_DEFAULT") => OnDelete::SetDefault,
        Some(other) => return Err(err(Code::File, at, format!("unknown on_delete `{other}`"))),
    })
}

/// The v1 snapshot as v2 state for `b`.
fn state_of(snap: &SchemaSnapshot, b: Backend, at: &str) -> Result<StateDoc, LoadError> {
    if !snap.m2m_tables.is_empty() || !snap.excludes.is_empty() {
        return Err(err(
            Code::NotEnabled,
            at,
            "many-to-many tables and exclusion constraints are adopted from a later release",
        ));
    }
    let d = dialect(b);
    let mut doc = StateDoc::default();
    let mut externs: BTreeMap<String, ExternTable> = BTreeMap::new();
    for t in snap
        .tables
        .iter()
        .filter(|t| !t.name.starts_with(FRAMEWORK_PREFIX))
    {
        if !t.composite_fks.is_empty() {
            return Err(err(
                Code::NotEnabled,
                at,
                format!(
                    "table `{}`: composite FKs are adopted from a later release",
                    t.name
                ),
            ));
        }
        let mut state = table_of(t, snap, d, b, at)?;
        for f in &t.fields {
            let Some(rel) = &f.fk else { continue };
            if rel.to.starts_with(FRAMEWORK_PREFIX) {
                let e = externs
                    .entry(rel.to.clone())
                    .or_insert_with(|| ExternTable {
                        table: rel.to.clone(),
                        columns: Vec::new(),
                        unique_sets: Vec::new(),
                        owner: ExternOwner::System,
                    });
                if !e.columns.iter().any(|c| c.name == rel.on) {
                    e.columns.push(ExternColumn {
                        name: rel.on.clone(),
                        ty: f.ty.clone(),
                        unique: true,
                    });
                }
            }
        }
        state.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        doc.tables.insert(state.name.clone(), state);
    }
    doc.externs = externs;
    Ok(doc)
}

fn table_of(
    t: &TableSnapshot,
    snap: &SchemaSnapshot,
    d: &dyn crate::sql::Dialect,
    b: Backend,
    at: &str,
) -> Result<TableState, LoadError> {
    let mut columns = Vec::new();
    let mut indexes = Vec::new();
    let mut checks = Vec::new();
    let mut fks = Vec::new();
    for f in &t.fields {
        columns.push(column_of(f, d));
        if f.unique && !f.primary_key {
            // PG names an inline UNIQUE `<t>_<c>_key`, MySQL after the column.
            let name = match b {
                Backend::Mysql => f.column.clone(),
                _ => pg_object_name(&t.name, Some(&f.column), "key", |_| false),
            };
            indexes.push(IndexDef {
                name,
                columns: vec![f.column.clone()],
                unique: true,
                method: "btree".into(),
                predicate: None,
                include: Vec::new(),
                skipped_on: Vec::new(),
            });
        }
        if f.min.is_some() || f.max.is_some() {
            let q = d.quote_ident(&f.column);
            let mut parts = Vec::new();
            if let Some(min) = f.min {
                parts.push(format!("{q} >= {min}"));
            }
            if let Some(max) = f.max {
                parts.push(format!("{q} <= {max}"));
            }
            checks.push(CheckDef {
                name: pg_object_name(&t.name, Some(&f.column), "check", |_| false),
                expr: parts.join(" AND "),
            });
        }
        if let Some(rel) = &f.fk {
            let name = format!("{}_{}_fkey", t.name, f.column);
            fks.push(FkDef {
                name,
                columns: vec![f.column.clone()],
                to: rel.to.clone(),
                on: vec![rel.on.clone()],
                on_delete: on_delete(rel.on_delete.as_deref(), at)?,
                to_ty: None,
            });
        }
    }
    for i in snap.indexes.iter().filter(|i| i.table == t.name) {
        indexes.push(IndexDef {
            name: i.name.clone(),
            columns: i.columns.clone(),
            unique: i.unique,
            method: if i.method.is_empty() {
                "btree".into()
            } else {
                i.method.to_ascii_lowercase()
            },
            predicate: i.where_clause.clone(),
            include: i.include.clone(),
            skipped_on: Vec::new(),
        });
    }
    for c in snap.checks.iter().filter(|c| c.table == t.name) {
        checks.push(CheckDef {
            name: c.name.clone(),
            expr: c.expr.clone(),
        });
    }
    // MySQL adds an index named after an FK no index covers.
    if b == Backend::Mysql {
        for fk in &fks {
            let pk: Vec<&str> = t
                .fields
                .iter()
                .filter(|f| f.primary_key)
                .map(|f| f.column.as_str())
                .collect();
            let covered = pk.first().copied() == fk.columns.first().map(String::as_str)
                || indexes
                    .iter()
                    .any(|i| i.columns.first() == fk.columns.first());
            if !covered {
                indexes.push(IndexDef {
                    name: fk.name.clone(),
                    columns: fk.columns.clone(),
                    unique: false,
                    method: "btree".into(),
                    predicate: None,
                    include: Vec::new(),
                    skipped_on: Vec::new(),
                });
            }
        }
    }
    let has_pk = t.fields.iter().any(|f| f.primary_key);
    let auto = t.fields.iter().find(|f| f.auto && f.primary_key);
    Ok(TableState {
        name: t.name.clone(),
        scope: Target::Tenant,
        columns,
        indexes,
        checks,
        fks,
        comment: t.db_table_comment.clone(),
        pg_pkey: has_pk.then(|| pg_object_name(&t.name, None, "pkey", |_| false)),
        pg_sequence: auto.map(|f| pg_object_name(&t.name, Some(&f.column), "seq", |_| false)),
    })
}

fn column_of(f: &FieldSnapshot, d: &dyn crate::sql::Dialect) -> ColumnDef {
    ColumnDef {
        column: f.column.clone(),
        ty: f.ty.clone(),
        nullable: f.nullable,
        max_length: f.max_length,
        primary_key: f.primary_key,
        auto: f.auto && f.primary_key,
        // The text engine v1 put in the DDL on this backend.
        default: f
            .default
            .as_deref()
            .map(|e| crate::migrate::diff::render_column_default(e, &f.ty, f.max_length, d)),
        generated_as: f.generated_as.clone(),
        comment: f.db_comment.clone(),
        case_insensitive: f.case_insensitive,
        sealed: false,
    }
}
