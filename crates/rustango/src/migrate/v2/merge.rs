//! `make --merge` and `make --rebase` (4.5): two leaves of one app become
//! one, through a merge node when the branches commute, or by moving one
//! leaf on top of the other.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::load::{load_file, Loaded};
use super::state::Target;
use super::{Code, LoadError};

fn err(code: Code, at: impl Into<String>, msg: impl Into<String>) -> LoadError {
    LoadError::new(code, at, msg)
}

/// The folder's files, read one by one (the project does not load with
/// two leaves).
fn folder_files(folder: &Path) -> Result<Vec<Loaded>, LoadError> {
    let at = folder.display().to_string();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(folder)
        .map_err(|e| err(Code::Io, &at, e.to_string()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p)
                .map_err(|e| err(Code::Io, p.display().to_string(), e.to_string()))?;
            load_file(p, &text)
        })
        .collect()
}

/// Each target's leaves in the folder: files no other file names as parent.
fn leaves(files: &[Loaded]) -> BTreeMap<Target, Vec<&Loaded>> {
    let mut out: BTreeMap<Target, Vec<&Loaded>> = BTreeMap::new();
    for f in files {
        let child = files.iter().any(|o| {
            o.file.target == f.file.target && o.file.parents.iter().any(|p| *p == f.file.id)
        });
        if !child {
            out.entry(f.file.target).or_default().push(f);
        }
    }
    out
}

fn next_index(files: &[Loaded]) -> u32 {
    files
        .iter()
        .filter_map(|f| crate::migrate::file::extract_index(&f.file.id))
        .max()
        .map_or(1, |n| n + 1)
}

fn write(path: &Path, body: &Value) -> Result<(), LoadError> {
    let text = format!("{}\n", serde_json::to_string_pretty(body).expect("json"));
    std::fs::write(path, text).map_err(|e| err(Code::Io, path.display().to_string(), e.to_string()))
}

/// Write `<n>_merge.json` over the folder's two (or more) leaves of one
/// target, then load the project: branches that do not commute remove it
/// and refuse, both ops named.
///
/// # Errors
/// Fewer than two leaves, a load error, or [`Code::Conflict`].
pub(crate) fn merge(dir: &Path, folder: &Path) -> Result<PathBuf, LoadError> {
    let files = folder_files(folder)?;
    let all = leaves(&files);
    let Some((target, ls)) = all.iter().find(|(_, l)| l.len() > 1) else {
        return Err(err(
            Code::Graph,
            folder.display().to_string(),
            "nothing to merge: every target has one leaf",
        ));
    };
    let id = format!("{:04}_merge", next_index(&files));
    let path = folder.join(format!("{id}.json"));
    let mut parents: Vec<&str> = ls.iter().map(|l| l.file.id.as_str()).collect();
    parents.sort_unstable();
    let body = serde_json::json!({
        "engine": 2, "format": 1, "id": id, "app": ls[0].file.app,
        "target": target.as_str(), "parents": parents, "operations": [],
    });
    write(&path, &body)?;
    if let Err(e) = super::load::load_project(dir) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// Move leaf `id` on top of the folder's other leaf: renumbered to the
/// next index, its parents reset. Refused when `applied` says a database
/// ran it or `shared` says it is on a remote branch, unless `force`.
///
/// # Errors
/// Not a leaf, not exactly two leaves, a refusal, or a load error.
pub(crate) fn rebase(
    dir: &Path,
    folder: &Path,
    id: &str,
    applied: bool,
    shared: bool,
    force: bool,
) -> Result<PathBuf, LoadError> {
    let files = folder_files(folder)?;
    let all = leaves(&files);
    let mine = files.iter().find(|f| f.file.id == id).ok_or_else(|| {
        err(
            Code::Graph,
            folder.display().to_string(),
            format!("no migration `{id}` here"),
        )
    })?;
    let ls = all.get(&mine.file.target).cloned().unwrap_or_default();
    if !ls.iter().any(|l| l.file.id == id) || ls.len() != 2 {
        return Err(err(
            Code::Graph,
            &mine.at,
            "rebase moves one of exactly two leaves of a target",
        ));
    }
    if !force && (applied || shared) {
        let why = if applied {
            "a database already ran it"
        } else {
            "it is on a remote branch"
        };
        return Err(err(
            Code::State,
            &mine.at,
            format!("`{id}` cannot be rebased: {why}; pass --force if every copy is yours"),
        ));
    }
    let onto = ls.iter().find(|l| l.file.id != id).expect("two leaves");
    let slug = id.split_once('_').map_or(id, |(_, s)| s);
    let new_id = format!("{:04}_{slug}", next_index(&files));
    let original =
        std::fs::read_to_string(&mine.path).map_err(|e| err(Code::Io, &mine.at, e.to_string()))?;
    let mut body: Value =
        serde_json::from_str(&original).map_err(|e| err(Code::File, &mine.at, e.to_string()))?;
    body["id"] = Value::String(new_id.clone());
    body["parents"] = serde_json::json!([onto.file.id]);
    let path = folder.join(format!("{new_id}.json"));
    write(&path, &body)?;
    let old = mine.path.clone();
    std::fs::remove_file(&old).map_err(|e| err(Code::Io, &mine.at, e.to_string()))?;
    if let Err(e) = super::load::load_project(dir) {
        // Put the file back as it was.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::write(&old, &original);
        return Err(e);
    }
    Ok(path)
}

/// Whether git knows `path` on a remote branch; `false` without git.
pub(crate) fn on_remote_branch(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "--remotes", "--oneline", "-1", "--"])
        .arg(path)
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}
