//! The v2 project loader: folders → files → checksums → decoded files →
//! one ordered DAG per target, then replay with state hashes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::checksum::checksum;
use super::format::{decode, File};
use super::graph::{order, Node};
use super::json::JsonError;
use super::replay::{apply_file, Ctx};
use super::state::{HashMemo, State, Target};
use super::{Code, LoadError};
use crate::migrate::engine::{read_folder, FileEngine, ProjectEngine, RawFile};

/// The longest slug a file name may carry.
const MAX_SLUG: usize = 50;

/// One loaded file.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub(crate) path: PathBuf,
    /// The path as errors print it.
    pub(crate) at: String,
    /// Checksum alg 1 of the raw file.
    pub(crate) checksum: String,
    /// The raw file in canonical form: what the ledger stores as `applied_body`.
    pub(crate) body: String,
    pub(crate) file: File,
}

impl crate::migrate::chains::ChainFacts for Loaded {
    fn key(&self) -> String {
        format!("{}:{}", self.file.app, self.file.id)
    }

    /// Schema ops only: raw SQL is opaque, as in engine v1.
    fn facts(&self) -> Vec<crate::migrate::chains::Fact<'_>> {
        use super::format::Op;
        use crate::migrate::chains::Fact;
        let mut out = Vec::new();
        if let Some(doc) = &self.file.state {
            out.extend(doc.tables.keys().map(|t| Fact::Created(t)));
        }
        for env in &self.file.operations {
            if let Op::DataSql { tables, .. } | Op::DataRust { tables, .. } = &env.op {
                out.extend(tables.write_tables().map(Fact::Written));
                continue;
            }
            let written = match &env.op {
                Op::CreateTable { table, .. } | Op::AdoptTable { table, .. } => {
                    out.push(Fact::Created(&table.name));
                    &table.name
                }
                Op::DropTable { table, .. } => {
                    out.push(Fact::Dropped(table));
                    table
                }
                Op::RenameTable { from, to } => {
                    out.push(Fact::Renamed { from, to });
                    to
                }
                Op::AlterTableComment { table, .. }
                | Op::AddColumn { table, .. }
                | Op::DropColumn { table, .. }
                | Op::RenameColumn { table, .. }
                | Op::AlterColumn { table, .. }
                | Op::AddFk { table, .. }
                | Op::DropFk { table, .. }
                | Op::AddIndex { table, .. }
                | Op::DropIndex { table, .. }
                | Op::RenameIndex { table, .. }
                | Op::AddCheck { table, .. }
                | Op::DropCheck { table, .. } => table,
                Op::RawSql { .. }
                | Op::StateOnly { .. }
                | Op::CreateExtension { .. }
                | Op::ExternTable { .. }
                | Op::DataSql { .. }
                | Op::DataRust { .. }
                | Op::ArchiveRestore { .. }
                | Op::ArchiveDrop { .. } => continue,
            };
            out.push(Fact::Written(written));
        }
        out
    }
}

/// A loaded project: its files and the canonical order of each target.
#[derive(Debug, Default)]
pub(crate) struct Project {
    pub(crate) files: Vec<Loaded>,
    pub(crate) order: BTreeMap<Target, Vec<usize>>,
}

/// Each target's final state, and the state hash before and after each file.
#[derive(Debug, Default)]
pub(crate) struct Replayed {
    pub(crate) states: BTreeMap<Target, State>,
    /// Per file index: `(state_before_hash, state_after_hash)`.
    pub(crate) hashes: BTreeMap<usize, (String, String)>,
}

fn engine_error(e: &crate::migrate::MigrateError) -> LoadError {
    LoadError::new(Code::Engine, "project", e.to_string())
}

/// Load the engine v2 project around the migrations dir `dir`.
///
/// # Errors
/// [`Code::Engine`] for a v1 project or a v2 file in `system`,
/// [`Code::V1File`] for a v1 file in a v2 project, and every file and
/// graph error.
pub(crate) fn load_project(dir: &Path) -> Result<Project, LoadError> {
    crate::migrate::engine::refuse_v2_in_system(dir).map_err(|e| engine_error(&e))?;
    let folders = crate::migrate::file::project_migration_dirs(dir);
    let mut raw: Vec<(PathBuf, Vec<RawFile>)> = Vec::with_capacity(folders.len());
    for f in folders {
        let files = read_folder(&f)
            .map_err(|e| LoadError::new(Code::Io, f.display().to_string(), e.to_string()))?;
        raw.push((f, files));
    }
    let all: Vec<&RawFile> = raw.iter().flat_map(|(_, fs)| fs).collect();
    // A file that does not parse decides no engine: report it as it is.
    if let Some(f) = all.iter().find(|f| f.engine == FileEngine::Unparsable) {
        load_file(&f.path, &f.text)?;
    }
    match ProjectEngine::of_files(all.iter().copied()) {
        ProjectEngine::V1 if all.is_empty() => {}
        ProjectEngine::V1 => {
            return Err(LoadError::new(
                Code::Engine,
                dir.display().to_string(),
                "this project is on engine v1: use `manage migrate` and the other v1 commands, \
                 or run `manage dbmigrate adopt` to switch it to engine v2",
            ))
        }
        ProjectEngine::Mixed { v1, .. } => {
            return Err(LoadError::new(
                Code::V1File,
                v1[0].display().to_string(),
                format!(
                    "written for engine v1 after the switch: delete it and re-run makemigrations \
                     ({} v1 file(s): {})",
                    v1.len(),
                    crate::migrate::engine::path_list(&v1)
                ),
            ))
        }
        _ => {}
    }
    load_folders(raw)
}

/// Load `embed_migrations!` entries (`(stem, text)`) as one v2 folder.
///
/// # Errors
/// [`Code::Engine`] when an entry is not engine v2, and every file and
/// graph error.
pub(crate) fn load_embedded(entries: &[(&str, &str)]) -> Result<Project, LoadError> {
    let folder = PathBuf::from("embedded");
    let mut files = Vec::with_capacity(entries.len());
    for (stem, text) in entries {
        let path = folder.join(format!("{stem}.json"));
        let engine = FileEngine::of(text);
        if engine != FileEngine::V2 {
            if engine == FileEngine::Unparsable {
                load_file(&path, text)?;
            }
            return Err(LoadError::new(
                Code::Engine,
                path.display().to_string(),
                "an engine v1 migration: embedded engine v2 migrations are all v2",
            ));
        }
        files.push(RawFile {
            path,
            text: (*text).to_owned(),
            engine,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    load_folders(vec![(folder, files)])
}

/// Load already-read v2 folders: one app label per folder, then the graphs.
fn load_folders(raw: Vec<(PathBuf, Vec<RawFile>)>) -> Result<Project, LoadError> {
    let mut project = Project::default();
    let mut labels: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (folder, files) in raw {
        let mut label: Option<String> = None;
        for f in files {
            debug_assert_eq!(f.engine, FileEngine::V2);
            let loaded = load_file(&f.path, &f.text)?;
            match &label {
                None => {
                    if let Some(other) = labels.get(&loaded.file.app) {
                        return Err(LoadError::new(
                            Code::AppLabel,
                            loaded.at,
                            format!(
                                "app `{}` is also the label of {}; one label per folder",
                                loaded.file.app,
                                other.display()
                            ),
                        ));
                    }
                    labels.insert(loaded.file.app.clone(), folder.clone());
                    label = Some(loaded.file.app.clone());
                }
                Some(l) if *l != loaded.file.app => {
                    return Err(LoadError::new(
                        Code::AppLabel,
                        loaded.at,
                        format!("app `{}` in a folder labelled `{l}`", loaded.file.app),
                    ))
                }
                Some(_) => {}
            }
            project.files.push(loaded);
        }
    }
    project.order = order_targets(&project.files)?;
    Ok(project)
}

/// One DAG per target, each in canonical order.
fn order_targets(files: &[Loaded]) -> Result<BTreeMap<Target, Vec<usize>>, LoadError> {
    let mut out = BTreeMap::new();
    for target in [Target::Registry, Target::Tenant] {
        let idx: Vec<usize> = (0..files.len())
            .filter(|&i| files[i].file.target == target)
            .collect();
        if idx.is_empty() {
            continue;
        }
        let nodes: Vec<Node<'_>> = idx
            .iter()
            .map(|&i| {
                let f = &files[i].file;
                Node {
                    app: &f.app,
                    id: &f.id,
                    target,
                    parents: &f.parents,
                    depends_on: &f.depends_on,
                    at: &files[i].at,
                }
            })
            .collect();
        let sorted = order(&nodes)?;
        out.insert(target, sorted.into_iter().map(|k| idx[k]).collect());
    }
    Ok(out)
}

/// Load one file: its name, checksum and content.
///
/// # Errors
/// [`Code::Name`], [`Code::File`], [`Code::NotInteger`], [`Code::Newer`],
/// [`Code::NotEnabled`], [`Code::Checkpoint`].
pub(crate) fn load_file(path: &Path, text: &str) -> Result<Loaded, LoadError> {
    let at = path.display().to_string();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| LoadError::new(Code::Name, &at, "the file name is not UTF-8"))?;
    check_name(stem, &at)?;
    let raw: Value = super::json::parse(text).map_err(|e| match e {
        JsonError::Syntax(msg) => LoadError::new(Code::File, &at, msg),
        JsonError::Duplicate { pointer } => LoadError::new(
            Code::File,
            format!("{at}#{pointer}"),
            "this key is written twice in one object",
        ),
    })?;
    let checksum = checksum(&raw).map_err(|e| {
        LoadError::new(
            Code::NotInteger,
            format!("{at}#{}", e.pointer),
            "numbers must be integers; write decimals as strings",
        )
    })?;
    let file = decode(&raw, &at)?;
    if file.id != stem {
        return Err(LoadError::new(
            Code::Name,
            format!("{at}#/id"),
            format!("`id` is `{}` but the file stem is `{stem}`", file.id),
        ));
    }
    if let (Some(base), Some(doc)) = (&file.base, &file.state) {
        let hash = State::from_doc(doc.clone()).hash(&mut HashMemo::default());
        if hash != base.state_sha256 {
            return Err(LoadError::new(
                Code::Checkpoint,
                format!("{at}#/base/state_sha256"),
                format!("the state hashes to {hash}; the checkpoint was edited"),
            ));
        }
    }
    let body = super::checksum::canonical(&raw).expect("checked for integers above");
    Ok(Loaded {
        path: path.to_path_buf(),
        at,
        checksum,
        body,
        file,
    })
}

/// `<NNNN>_<slug>`: four or more digits, a lowercase slug of at most 50
/// bytes, never `auto`.
fn check_name(stem: &str, at: &str) -> Result<(), LoadError> {
    let bad = |why: &str| {
        Err(LoadError::new(
            Code::Name,
            at,
            format!("`{stem}`: {why}; name files `<NNNN>_<slug>.json`"),
        ))
    };
    let Some((num, slug)) = stem.split_once('_') else {
        return bad("no `_` after the number");
    };
    if num.len() < 4 || !num.bytes().all(|b| b.is_ascii_digit()) {
        return bad("the number has fewer than four digits");
    }
    if slug.is_empty()
        || slug.len() > MAX_SLUG
        || !slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return bad("the slug must be 1 to 50 of a-z, 0-9 and _");
    }
    if slug == "auto" {
        return bad("the slug is never `auto`");
    }
    Ok(())
}

/// Replay each target in canonical order, registry first, hashing state
/// before and after each file.
///
/// # Errors
/// The first op that does not replay.
pub(crate) fn replay(project: &Project) -> Result<Replayed, LoadError> {
    let mut memo = HashMemo::default();
    let mut hashes = BTreeMap::new();
    let mut last: Option<(Target, String)> = None;
    let empty = State::default().hash(&mut memo);
    let states = replay_with(project, |target, i, after| {
        let prev = match &last {
            Some((t, h)) if *t == target => h.clone(),
            _ => empty.clone(),
        };
        let now = after.hash(&mut memo);
        hashes.insert(i, (prev, now.clone()));
        last = Some((target, now));
    })?;
    Ok(Replayed { states, hashes })
}

/// Replay each target in canonical order, registry first; `each` sees the
/// state after every file.
///
/// # Errors
/// The first op that does not replay.
pub(crate) fn replay_with(
    project: &Project,
    mut each: impl FnMut(Target, usize, &State),
) -> Result<BTreeMap<Target, State>, LoadError> {
    let mut states: BTreeMap<Target, State> = BTreeMap::new();
    for (&target, idx) in &project.order {
        let mut state = State::default();
        for &i in idx {
            let ctx = Ctx {
                target,
                registry: states.get(&Target::Registry),
            };
            let f = &project.files[i];
            apply_file(&mut state, &f.file, &f.at, &ctx)?;
            each(target, i, &state);
        }
        states.insert(target, state);
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, v: &Value) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), v.to_string()).unwrap();
    }

    fn file(app: &str, id: &str, parents: &[&str], ops: Value) -> Value {
        json!({"engine": 2, "format": 1, "id": id, "app": app, "parents": parents,
               "target": "tenant", "operations": ops})
    }

    fn table(name: &str) -> Value {
        json!({"op": "create_table", "v": 1, "table": {"name": name, "scope": "tenant",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}})
    }

    #[test]
    fn a_project_loads_and_replays() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        write(
            &dir,
            "0001_a",
            &file("project", "0001_a", &[], json!([table("a")])),
        );
        write(
            &dir,
            "0002_b",
            &file("project", "0002_b", &["0001_a"], json!([table("b")])),
        );
        let blog = tmp.path().join("blog/migrations");
        let mut post = file("blog", "0001_post", &[], json!([table("post")]));
        post["depends_on"] = json!(["project:0002_b"]);
        write(&blog, "0001_post", &post);
        // A v1 system chain beside it does not decide the engine.
        write(
            &tmp.path().join("system/migrations"),
            "0001_s",
            &json!({"name": "0001_s", "created_at": "", "snapshot": {"tables": []}, "forward": []}),
        );
        let p = load_project(&dir).unwrap();
        let order: Vec<&str> = p.order[&Target::Tenant]
            .iter()
            .map(|&i| p.files[i].file.id.as_str())
            .collect();
        assert_eq!(order, ["0001_a", "0002_b", "0001_post"]);
        assert!(p
            .files
            .iter()
            .all(|f| f.path.extension().is_some_and(|e| e == "json")));
        let r = replay(&p).unwrap();
        let tables: Vec<&str> = r.states[&Target::Tenant]
            .tables
            .keys()
            .map(|k| &**k)
            .collect();
        assert_eq!(tables, ["a", "b", "post"]);
        // Each file's before-hash is the previous file's after-hash.
        let (_, after_a) = &r.hashes[&p.order[&Target::Tenant][0]];
        let (before_b, _) = &r.hashes[&p.order[&Target::Tenant][1]];
        assert_eq!(after_a, before_b);
    }

    #[test]
    fn engine_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        // Empty: a project born on v2.
        assert!(load_project(&dir).unwrap().files.is_empty());
        let v1 = json!({"name": "0001_old", "created_at": "", "snapshot": {"tables": []},
                        "forward": []});
        write(&dir, "0001_old", &v1);
        assert_eq!(
            load_project(&dir).unwrap_err().code,
            Code::Engine,
            "v1 project"
        );
        write(&dir, "0002_a", &file("project", "0002_a", &[], json!([])));
        let e = load_project(&dir).unwrap_err();
        assert_eq!(e.code, Code::V1File);
        assert!(
            e.msg.contains("written for engine v1 after the switch"),
            "{e}"
        );
        std::fs::remove_file(dir.join("0001_old.json")).unwrap();
        // A v2 file with a typo is a parse error, not "delete it" or "adopt".
        std::fs::write(dir.join("0003_typo.json"), r#"{"engine": 2, "format": 1,}"#).unwrap();
        let e = load_project(&dir).unwrap_err();
        assert_eq!(e.code, Code::File, "{e}");
        assert!(e.at.ends_with("0003_typo.json"), "{e}");
        // Alone it is still a parse error, not "this project is on engine v1".
        std::fs::rename(dir.join("0002_a.json"), tmp.path().join("aside.json")).unwrap();
        assert_eq!(load_project(&dir).unwrap_err().code, Code::File);
        std::fs::rename(tmp.path().join("aside.json"), dir.join("0002_a.json")).unwrap();
        std::fs::remove_file(dir.join("0003_typo.json")).unwrap();
        write(
            &tmp.path().join("system/migrations"),
            "0001_s",
            &file("project", "0001_s", &[], json!([])),
        );
        assert_eq!(
            load_project(&dir).unwrap_err().code,
            Code::Engine,
            "v2 in system"
        );
    }

    #[test]
    fn names_and_labels() {
        let ok = file("project", "0001_add_bio", &[], json!([]));
        for (name, code) in [
            ("001_add_bio", Code::Name),
            ("0001_auto", Code::Name),
            ("0001_Add", Code::Name),
            ("0001", Code::Name),
        ] {
            let mut v = ok.clone();
            v["id"] = json!(name);
            let e = load_file(Path::new(&format!("{name}.json")), &v.to_string()).unwrap_err();
            assert_eq!(e.code, code, "{name}");
        }
        let e = load_file(Path::new("0002_other.json"), &ok.to_string()).unwrap_err();
        assert_eq!(e.code, Code::Name, "id differs from the stem");
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        write(&dir, "0001_a", &file("project", "0001_a", &[], json!([])));
        write(&dir, "0002_b", &file("other", "0002_b", &[], json!([])));
        assert_eq!(load_project(&dir).unwrap_err().code, Code::AppLabel);
    }

    /// A key written twice is a load error, not "the last one wins".
    #[test]
    fn duplicate_keys_are_load_errors() {
        let text = r#"{"engine": 2, "format": 1, "id": "0001_a", "app": "project",
            "target": "tenant", "operations": [{"op": "add_check", "v": 1, "table": "t",
            "name": "c", "expr": "x > 0", "expr": "x > 1"}]}"#;
        let e = load_file(Path::new("0001_a.json"), text).unwrap_err();
        assert_eq!(e.code, Code::File);
        assert!(e.at.ends_with("0001_a.json#/operations/0/expr"), "{e}");
    }

    #[test]
    fn floats_are_load_errors() {
        let mut v = file("project", "0001_a", &[], json!([]));
        v["allow"] = json!([{"code": "x", "reason": "y", "n": 1.5}]);
        let e = load_file(Path::new("0001_a.json"), &v.to_string()).unwrap_err();
        assert_eq!(e.code, Code::NotInteger);
        assert!(e.at.ends_with("#/allow/0/n"), "{e}");
    }

    /// An edited adopt checkpoint is caught by its state hash.
    #[test]
    fn checkpoint_hash_is_verified() {
        let doc = super::super::state::StateDoc::default();
        let hash = State::from_doc(doc).hash(&mut HashMemo::default());
        let mut v = json!({"engine": 2, "format": 1, "kind": "checkpoint", "id": "0011_adopt",
            "app": "project", "target": "tenant", "operations": [],
            "base": {"legacy": "0010_x", "state_sha256": hash}, "state": {"tables": {}}});
        load_file(Path::new("0011_adopt.json"), &v.to_string()).unwrap();
        v["state"]["extensions"] = json!(["citext"]);
        let e = load_file(Path::new("0011_adopt.json"), &v.to_string()).unwrap_err();
        assert_eq!(e.code, Code::Checkpoint);
    }
}
