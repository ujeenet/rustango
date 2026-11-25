//! `adopt`, the offline part (9.2): one checkpoint per app folder from the
//! last engine v1 snapshot, then the v1 files go (git keeps them).
//!
//! The state is written for one backend: defaults as engine v1 rendered
//! them there, and the names v1 left to that backend where it has them.
//! Names a backend never stores (an unnamed SQLite constraint) get v1's
//! PG spelling; the drift check matches those by shape.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::state::{Backend, HashMemo, IndexDef, State, StateDoc};
use super::{Code, LoadError};
use crate::migrate::engine::ProjectEngine;
use crate::migrate::file::{self, Migration, MigrationScope, Operation};
use crate::migrate::snapshot::SchemaSnapshot;

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
pub(super) fn app_label(root: &Path, folder: &Path) -> String {
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

/// The v1 snapshot as v2 state for `b`: the models state, plus the index
/// MySQL added for an FK nothing covers, named after the FK.
fn state_of(snap: &SchemaSnapshot, b: Backend, at: &str) -> Result<StateDoc, LoadError> {
    let mut doc = super::make::state_of(snap, &[b], at)?;
    if b == Backend::Mysql {
        for t in doc.tables.values_mut() {
            let pk = t.primary_key().next().map(str::to_owned);
            for fk in t.fks.clone() {
                let covered = pk.as_deref() == fk.columns.first().map(String::as_str)
                    || t.indexes
                        .iter()
                        .any(|i| i.columns.first() == fk.columns.first());
                if !covered {
                    t.indexes.push(IndexDef {
                        name: fk.name.clone(),
                        columns: fk.columns.clone(),
                        unique: false,
                        method: "btree".into(),
                        predicate: None,
                        include: Vec::new(),
                        skipped_on: Vec::new(),
                        constraint: false,
                    });
                }
            }
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }
    Ok(doc)
}
