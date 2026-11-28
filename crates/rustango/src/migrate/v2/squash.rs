//! `squash` and `squash --prune` (9.6): an app's chain from its root to
//! `<to>` becomes one checkpoint holding the state, the replaced ids and
//! the data ops a fresh install runs; prune deletes the replaced files.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::commute::{keys, Key};
use super::format::{Kind, OnFresh, Op};
use super::invert::{invert, Allow};
use super::load::{load_project, Project};
use super::replay::{apply_file, Ctx};
use super::state::{HashMemo, State, Target};
use super::{Code, LoadError};

fn err(code: Code, at: impl Into<String>, msg: impl Into<String>) -> LoadError {
    LoadError::new(code, at, msg)
}

/// Every file `i` reaches through `parents` and `depends_on`, itself included.
fn ancestors(project: &Project, target: Target, i: usize) -> HashSet<usize> {
    let order = &project.order[&target];
    let key = |n: usize| {
        (
            project.files[n].file.app.as_str(),
            project.files[n].file.id.as_str(),
        )
    };
    let mut seen = HashSet::new();
    let mut stack = vec![i];
    while let Some(n) = stack.pop() {
        if !seen.insert(n) {
            continue;
        }
        let f = &project.files[n].file;
        let up: Vec<(&str, &str)> = f
            .parents
            .iter()
            .map(|p| (f.app.as_str(), p.as_str()))
            .chain(f.depends_on.iter().map(|(a, d)| (a.as_str(), d.as_str())))
            .collect();
        stack.extend(order.iter().copied().filter(|&m| up.contains(&key(m))));
    }
    seen
}

/// Tables an app's files own after `range`, in order.
fn owned_tables(project: &Project, range: &[usize]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for &i in range {
        let f = &project.files[i].file;
        if let Some(doc) = &f.state {
            out.extend(doc.tables.keys().cloned());
        }
        for env in &f.operations {
            match &env.op {
                Op::CreateTable { table, .. } | Op::AdoptTable { table, .. } => {
                    out.insert(table.name.clone());
                }
                Op::DropTable { table, .. } => {
                    out.remove(table);
                }
                Op::RenameTable { from, to } => {
                    if out.remove(from) {
                        out.insert(to.clone());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn touches_table(k: &Key, tables: &BTreeSet<String>) -> bool {
    match k {
        Key::Table(t) | Key::Column(t, _) => tables.contains(t),
        Key::All => true,
        _ => false,
    }
}

/// Write the squash checkpoint of `app`'s chain from its root through
/// `to`, then load the project: its tip state must equal the one before.
///
/// # Errors
/// `from` is not the root, a range that is not every ancestor of `to`, a
/// kept data op on a table changed later in the range, another app's file
/// writing this app's tables, or a self-check mismatch.
#[allow(clippy::too_many_lines)]
pub(crate) fn squash(
    dir: &Path,
    folder: &Path,
    app: &str,
    from: &str,
    to: &str,
) -> Result<PathBuf, LoadError> {
    let at = folder.display().to_string();
    let project = load_project(dir)?;
    let pos = project
        .files
        .iter()
        .position(|l| l.file.app == app && l.file.id == to);
    let Some(to_i) = pos else {
        return Err(err(Code::Graph, &at, format!("no migration `{app}:{to}`")));
    };
    let target = project.files[to_i].file.target;
    let order = project.order[&target].clone();
    let chain: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| project.files[i].file.app == app)
        .collect();
    if project.files[chain[0]].file.id != from {
        return Err(err(
            Code::Graph,
            &at,
            format!(
                "a squash starts at the app's root `{}`, not `{from}`",
                project.files[chain[0]].file.id
            ),
        ));
    }
    let up = ancestors(&project, target, to_i);
    let cut = chain.iter().position(|&i| i == to_i).expect("in chain");
    let range: Vec<usize> = chain[..=cut].to_vec();
    if let Some(&stray) = range.iter().find(|i| !up.contains(i)) {
        return Err(err(
            Code::Graph,
            &project.files[stray].at,
            format!(
                "`{}` is not an ancestor of `{to}`: squash up to a node every earlier one leads to",
                project.files[stray].file.id
            ),
        ));
    }
    let tables = owned_tables(&project, &range);
    // Another app's ancestor writing these tables would replay twice.
    for &i in order
        .iter()
        .filter(|i| up.contains(i) && !range.contains(i))
    {
        let l = &project.files[i];
        for (n, env) in l.file.operations.iter().enumerate() {
            let k = keys(&env.op, &format!("{}#/operations/{n}", l.at))?;
            if let Some(w) = k.writes.iter().find(|w| touches_table(w, &tables)) {
                return Err(err(
                    Code::Graph,
                    &l.at,
                    format!("`{}:{}` writes {w} of app `{app}`: squash after it, or move the change into `{app}`", l.file.app, l.file.id),
                ));
            }
        }
    }
    // State after the range, the kept data ops, and whether every op inverts.
    let ctx = Ctx {
        target,
        registry: None,
    };
    let mut state = State::default();
    let mut kept: Vec<Value> = Vec::new();
    let mut irreversible = false;
    let mut extensions: BTreeSet<String> = BTreeSet::new();
    let mut externs: BTreeSet<String> = BTreeSet::new();
    let range_set: HashSet<usize> = range.iter().copied().collect();
    let mut replaces: Vec<String> = Vec::new();
    for &i in order.iter().filter(|i| up.contains(i)) {
        let l = &project.files[i];
        let mine = range_set.contains(&i);
        if mine {
            if l.file.kind == Kind::Checkpoint {
                // Only real files are replaced: an older squash stands in
                // for its own, which a database may have run one by one.
                replaces.extend(l.file.replaces.iter().cloned());
                // Its state comes in whole; its kept data ops stay kept,
                // under the same rule as the range's own.
                irreversible = true;
                apply_file(&mut state, &l.file, &l.at, &ctx)?;
                if let Some(doc) = &l.file.state {
                    externs.extend(doc.externs.keys().cloned());
                    extensions.extend(doc.extensions.iter().cloned());
                }
                let raw: Value = serde_json::from_str(&l.body)
                    .map_err(|e| err(Code::File, &l.at, e.to_string()))?;
                for n in 0..l.file.operations.len() {
                    refuse_changed_later(&project, &range, i, n, &l.at)?;
                }
                kept.extend(raw["operations"].as_array().cloned().unwrap_or_default());
                continue;
            }
            replaces.push(l.file.id.clone());
            let raw: Value =
                serde_json::from_str(&l.body).map_err(|e| err(Code::File, &l.at, e.to_string()))?;
            for (n, env) in l.file.operations.iter().enumerate() {
                let mut after = state.clone();
                super::replay::apply(&mut after, &env.op, &l.at, &ctx, &[])?;
                if invert(&env.op, &after, Allow { data_loss: true }, &l.at).is_err() {
                    irreversible = true;
                }
                match &env.op {
                    Op::CreateExtension { name, .. } => {
                        extensions.insert(name.clone());
                    }
                    Op::ExternTable { table, .. } => {
                        externs.insert(table.clone());
                    }
                    _ => {}
                }
                let fresh = matches!(
                    env.op,
                    Op::DataSql {
                        on_fresh: OnFresh::Run,
                        ..
                    } | Op::DataRust {
                        on_fresh: OnFresh::Run,
                        ..
                    }
                );
                if fresh {
                    kept.push((raw["operations"][n]).clone());
                    refuse_changed_later(&project, &range, i, n, &l.at)?;
                }
                state = after;
            }
        } else {
            apply_file(&mut state, &l.file, &l.at, &ctx)?;
        }
    }
    let mut doc = state.normalised();
    doc.tables.retain(|t, _| tables.contains(t));
    doc.externs.retain(|t, _| externs.contains(t));
    doc.extensions.retain(|e| extensions.contains(e));
    let idx = crate::migrate::file::extract_index(to).unwrap_or(0);
    let id = format!("{idx:04}_squash");
    let mut body = json!({
        "engine": 2, "format": 1, "kind": "checkpoint", "id": id, "app": app,
        "target": target.as_str(), "parents": [], "replaces": replaces, "state": doc,
        "operations": kept,
        "description": format!("Squashed `{from}`..`{to}`. Rollback stops here."),
    });
    if irreversible {
        body["irreversible_below"] = json!(to);
    }
    if let Some(db) = &project.files[to_i].file.database {
        body["database"] = json!(db);
    }
    let path = folder.join(format!("{id}.json"));
    if path.exists() {
        return Err(err(Code::Io, path.display().to_string(), "the file exists"));
    }
    let before = super::load::replay(&project)?.states.remove(&target);
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&body).expect("json")),
    )
    .map_err(|e| err(Code::Io, path.display().to_string(), e.to_string()))?;
    // Self-check: the project with the checkpoint replays to the same tip.
    let check = load_project(dir).and_then(|p| super::load::replay(&p));
    let mut memo = HashMemo::default();
    let same = match &check {
        Ok(r) => {
            r.states.get(&target).map(|s| s.hash(&mut memo))
                == before.as_ref().map(|s| s.hash(&mut memo))
        }
        Err(_) => false,
    };
    if !same {
        let _ = std::fs::remove_file(&path);
        return Err(match check {
            Err(e) => e,
            Ok(_) => err(
                Code::State,
                &at,
                "internal: the squashed project replays to another state; nothing was written",
            ),
        });
    }
    Ok(path)
}

/// A kept data op must not touch a table a later op in the range changes:
/// on a fresh install it runs on the final state.
fn refuse_changed_later(
    project: &Project,
    range: &[usize],
    file: usize,
    op: usize,
    at: &str,
) -> Result<(), LoadError> {
    match changed_later(project, range, file, op)? {
        None => Ok(()),
        Some((later, w)) => Err(err(
            Code::Checkpoint,
            at,
            format!(
                "a kept data op of `{}` touches {w}, which `{}` changes later in the range: \
                 squash before that change",
                project.files[file].file.id, project.files[later].file.id
            ),
        )),
    }
}

/// The fresh fast path's rule (8.5): every fresh data op (`(file, op)` in
/// `order`) comes after the last schema op on the tables it names.
///
/// # Errors
/// A nested state that does not decode.
pub(crate) fn fresh_ops_follow_schema(
    project: &Project,
    order: &[usize],
    fresh: &[(usize, usize)],
) -> Result<bool, LoadError> {
    for &(file, op) in fresh {
        if changed_later(project, order, file, op)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The first op after data op `op` of `file` in `seq` that changes the
/// shape of a table the data op names: `(file, key)`.
fn changed_later(
    project: &Project,
    seq: &[usize],
    file: usize,
    op: usize,
) -> Result<Option<(usize, Key)>, LoadError> {
    let l = &project.files[file];
    let k = keys(&l.file.operations[op].op, &l.at)?;
    let tables: BTreeSet<String> = k
        .writes
        .iter()
        .chain(&k.reads)
        .filter_map(|k| match k {
            Key::Table(t) | Key::Column(t, _) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let start = seq.iter().position(|&i| i == file).expect("in sequence");
    for (si, &i) in seq.iter().enumerate().skip(start) {
        let m = &project.files[i];
        if si > start {
            // A checkpoint brings its tables in whole: a schema change.
            if let Some(t) = m
                .file
                .state
                .iter()
                .flat_map(|d| d.tables.keys())
                .find(|t| tables.contains(*t))
            {
                return Ok(Some((i, Key::Table(t.clone()))));
            }
        }
        for (n, env) in m.file.operations.iter().enumerate() {
            if (si == start && n <= op)
                || matches!(env.op, Op::DataSql { .. } | Op::DataRust { .. })
            {
                continue;
            }
            let later = keys(&env.op, &m.at)?;
            if let Some(w) = later.writes.into_iter().find(|w| touches_table(w, &tables)) {
                return Ok(Some((i, w)));
            }
        }
    }
    Ok(None)
}

/// What says the replaced files are no longer needed (9.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PruneGate {
    /// One database: whether it applied the checkpoint.
    Here { applied: bool },
    /// Under tenancy: the targets (registry or tenant slugs) that ran part
    /// of what the checkpoint replaces and not the checkpoint itself.
    #[cfg(feature = "tenancy")]
    Fleet { behind: Vec<String> },
    /// `--assume-all-applied`.
    Assumed,
}

/// Delete the files `checkpoint` replaces, when `gate` allows it.
///
/// # Errors
/// No such checkpoint, a gate that refuses, or a file that does not delete.
pub(crate) fn prune(
    dir: &Path,
    folder: &Path,
    checkpoint: &str,
    gate: &PruneGate,
) -> Result<Vec<PathBuf>, LoadError> {
    let project = load_project(dir)?;
    let at = folder.display().to_string();
    let Some(c) = squash_checkpoint(&project, checkpoint) else {
        return Err(err(
            Code::Graph,
            &at,
            format!("no squash checkpoint `{checkpoint}`"),
        ));
    };
    let refusal = match gate {
        PruneGate::Here { applied: false } => Some(format!(
            "`{checkpoint}` is not applied here: prune after every database ran it, or pass \
             --assume-all-applied"
        )),
        #[cfg(feature = "tenancy")]
        PruneGate::Fleet { behind } if !behind.is_empty() => Some(format!(
            "{} ran part of what `{checkpoint}` replaces and need those files: migrate them \
             first, or pass --assume-all-applied",
            behind.join(", ")
        )),
        _ => None,
    };
    if let Some(msg) = refusal {
        return Err(err(Code::State, &c.at, msg));
    }
    let mut removed = Vec::new();
    // Every file it stands for: the replaced ones and any older squash.
    let gone = project
        .replaced
        .iter()
        .filter(|((app, _), by)| *app == c.file.app && *by == &c.file.id)
        .map(|((_, id), _)| id);
    for id in gone {
        let p = folder.join(format!("{id}.json"));
        if p.exists() {
            std::fs::remove_file(&p)
                .map_err(|e| err(Code::Io, p.display().to_string(), e.to_string()))?;
            removed.push(p);
        }
    }
    Ok(removed)
}

/// The squash checkpoint `id` of `project`.
pub(crate) fn squash_checkpoint<'p>(
    project: &'p Project,
    id: &str,
) -> Option<&'p super::load::Loaded> {
    project
        .files
        .iter()
        .find(|l| l.file.id == id && !l.file.replaces.is_empty())
}

/// Whether a ledger holding `applied` ids of the checkpoint's app still
/// needs the replaced files: part of them ran, the checkpoint did not.
#[cfg(feature = "tenancy")]
pub(crate) fn needs_replaced(c: &super::format::File, applied: &HashSet<String>) -> bool {
    !applied.contains(&c.id)
        && c.replaces.iter().any(|r| applied.contains(r))
        && !c.replaces.iter().all(|r| applied.contains(r))
}
