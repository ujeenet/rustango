//! The v2 project loader: folders → files → checksums → decoded files →
//! one ordered DAG per target, then replay with state hashes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::checksum::checksum;
use super::format::{decode, File, Op};
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

/// A loaded project: its files and the canonical order of each target.
#[derive(Debug, Default)]
pub(crate) struct Project {
    pub(crate) files: Vec<Loaded>,
    pub(crate) order: BTreeMap<Target, Vec<usize>>,
    /// `(app, id)` a squash checkpoint replaces → that checkpoint's id
    /// (9.6). Their files, when still on disk, are not in `files`.
    pub(crate) replaced: BTreeMap<(String, String), String>,
}

/// Each target's final state, and the state hash before and after each file.
#[derive(Debug, Default)]
pub(crate) struct Replayed {
    pub(crate) states: BTreeMap<Target, State>,
    /// Per file index: `(state_before_hash, state_after_hash)`.
    #[cfg(test)]
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

/// The framework's system apps this build runs (design items 20-23), as
/// a project of their own: their ledger is the system ledger.
///
/// # Errors
/// A shipped system file that does not load or order.
pub(crate) fn system_project() -> Result<Project, LoadError> {
    let files = super::system::files()?;
    let order = order_targets(&files)?;
    Ok(Project {
        files,
        order,
        replaced: BTreeMap::new(),
    })
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
    refuse_framework_writes(&project.files)?;
    remap_replaced(&mut project);
    project.order = order_targets(&project.files)?;
    super::commute::refuse_conflicts(&project)?;
    Ok(project)
}

/// Framework tables belong to the system chain (design item 20): a project
/// file may name one through `extern_table` and write its rows, never
/// create, alter, drop or rename it.
fn refuse_framework_writes(files: &[Loaded]) -> Result<(), LoadError> {
    use super::commute::Key;
    use super::make::FRAMEWORK_PREFIX;
    for l in files {
        for (i, env) in l.file.operations.iter().enumerate() {
            if matches!(
                env.op,
                Op::ExternTable { .. } | Op::DataSql { .. } | Op::DataRust { .. }
            ) {
                continue;
            }
            let at = format!("{}#/operations/{i}", l.at);
            let keys = super::commute::keys(&env.op, &at)?;
            let hit = keys.writes.iter().find_map(|k| match k {
                Key::Table(t) | Key::Column(t, _) if t.starts_with(FRAMEWORK_PREFIX) => Some(t),
                _ => None,
            });
            if let Some(t) = hit {
                return Err(LoadError::new(
                    Code::FrameworkTable,
                    at,
                    format!(
                        "`{t}` is a framework table: the framework's system chain owns it; \
                         declare it with `extern_table` instead"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Squash checkpoints stand for the ids they replace, at load (9.6): the
/// replaced files leave the graph, and every `parents`, `depends_on` and
/// `requires` naming one names the checkpoint.
fn remap_replaced(project: &mut Project) {
    for l in &project.files {
        for id in &l.file.replaces {
            project
                .replaced
                .insert((l.file.app.clone(), id.clone()), l.file.id.clone());
        }
    }
    if project.replaced.is_empty() {
        return;
    }
    let replaced = &project.replaced;
    project
        .files
        .retain(|l| !replaced.contains_key(&(l.file.app.clone(), l.file.id.clone())));
    let to = |app: &str, id: &str| replaced.get(&(app.to_owned(), id.to_owned())).cloned();
    for l in &mut project.files {
        let app = l.file.app.clone();
        let mut parents: Vec<String> = Vec::new();
        for p in &l.file.parents {
            let p = to(&app, p).unwrap_or_else(|| p.clone());
            if !parents.contains(&p) {
                parents.push(p);
            }
        }
        l.file.parents = parents;
        for (a, d) in &mut l.file.depends_on {
            if let Some(c) = to(a, d) {
                *d = c;
            }
        }
        for r in &mut l.file.requires {
            if let Some((side, id)) = r.split_once(':') {
                // `requires` names another target; a checkpoint there has the app's ids.
                if let Some(c) = replaced
                    .iter()
                    .find(|((_, rid), _)| rid == id)
                    .map(|(_, c)| c.clone())
                {
                    *r = format!("{side}:{c}");
                }
            }
        }
    }
}

/// One DAG per target, each in canonical order. An app's `database` files
/// name one database, so each database's chain is its apps' files.
fn order_targets(files: &[Loaded]) -> Result<BTreeMap<Target, Vec<usize>>, LoadError> {
    let mut db_of: BTreeMap<&str, &str> = BTreeMap::new();
    for l in files.iter().filter(|l| l.file.target == Target::Database) {
        let db = l.file.database.as_deref().unwrap_or_default();
        if let Some(other) = db_of.insert(&l.file.app, db).filter(|o| *o != db) {
            return Err(LoadError::new(
                Code::File,
                format!("{}#/database", l.at),
                format!(
                    "app `{}` runs on database `{other}` and `{db}`; one database per app",
                    l.file.app
                ),
            ));
        }
    }
    let mut out = BTreeMap::new();
    for target in [
        Target::EveryDatabase,
        Target::Registry,
        Target::Tenant,
        Target::Database,
    ] {
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
    #[cfg(not(test))]
    let states = replay_with(project, |_, _, _| {})?;
    #[cfg(test)]
    let (states, hashes) = {
        let mut memo = HashMemo::default();
        let mut hashes = BTreeMap::new();
        let mut last: BTreeMap<(Target, Option<&str>), String> = BTreeMap::new();
        let empty = State::default().hash(&mut memo);
        let states = replay_with(project, |target, i, after| {
            let chain = (target, project.files[i].file.database.as_deref());
            let prev = last.get(&chain).cloned().unwrap_or_else(|| empty.clone());
            let now = after.hash(&mut memo);
            hashes.insert(i, (prev, now.clone()));
            last.insert(chain, now);
        })?;
        (states, hashes)
    };
    Ok(Replayed {
        states,
        #[cfg(test)]
        hashes,
    })
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
        // Each configured database has a state of its own.
        let mut by_db: BTreeMap<Option<&str>, State> = BTreeMap::new();
        for &i in idx {
            let ctx = Ctx {
                target,
                registry: states.get(&Target::Registry),
            };
            let f = &project.files[i];
            let state = by_db.entry(f.file.database.as_deref()).or_default();
            apply_file(state, &f.file, &f.at, &ctx)?;
            each(target, i, state);
        }
        if let Some((_, state)) = by_db.pop_last() {
            states.insert(target, state);
        }
    }
    Ok(states)
}

/// The files of `target`'s chain that run on configured database `db`
/// (`None` for the other targets), in canonical order.
pub(crate) fn chain<'p>(
    project: &'p Project,
    target: Target,
    db: Option<&'p str>,
) -> impl Iterator<Item = usize> + 'p {
    project
        .order
        .get(&target)
        .into_iter()
        .flatten()
        .copied()
        .filter(move |&i| project.files[i].file.database.as_deref() == db)
}

/// The configured databases `database` files name.
pub(crate) fn databases(project: &Project) -> std::collections::BTreeSet<&str> {
    project
        .files
        .iter()
        .filter_map(|l| l.file.database.as_deref())
        .collect()
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

    /// `every_database` and `database` files (design item 20): each target
    /// is a chain of its own, each configured database a state of its own.
    #[test]
    fn every_database_and_database_chains() {
        let on = |app: &str, id: &str, target: &str, db: Option<&str>, t: &str| {
            let mut v = file(app, id, &[], json!([table(t)]));
            v["target"] = json!(target);
            v["operations"][0]["table"]["scope"] = json!(target);
            if let Some(db) = db {
                v["database"] = json!(db);
            }
            v
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        write(
            &dir,
            "0001_marks",
            &on("project", "0001_marks", "every_database", None, "marks"),
        );
        write(
            &dir,
            "0002_a",
            &file("project", "0002_a", &[], json!([table("a")])),
        );
        // The same table name in two databases: separate states.
        let x = tmp.path().join("x/migrations");
        write(
            &x,
            "0001_t",
            &on("x", "0001_t", "database", Some("one"), "t"),
        );
        let y = tmp.path().join("y/migrations");
        write(
            &y,
            "0001_t",
            &on("y", "0001_t", "database", Some("two"), "t"),
        );
        let p = load_project(&dir).unwrap();
        let ids = |t: Target, db: Option<&str>| -> Vec<String> {
            chain(&p, t, db)
                .map(|i| format!("{}:{}", p.files[i].file.app, p.files[i].file.id))
                .collect()
        };
        assert_eq!(ids(Target::EveryDatabase, None), ["project:0001_marks"]);
        assert_eq!(ids(Target::Tenant, None), ["project:0002_a"]);
        assert_eq!(ids(Target::Database, Some("one")), ["x:0001_t"]);
        assert_eq!(ids(Target::Database, Some("two")), ["y:0001_t"]);
        let r = replay(&p).unwrap();
        let empty = State::default().hash(&mut HashMemo::default());
        for i in chain(&p, Target::Database, Some("two")) {
            assert_eq!(r.hashes[&i].0, empty, "a database's chain starts empty");
        }
        // Each rule, one file at a time.
        let bad = |v: Value, want: &str| {
            let e = load_file(Path::new("0003_z.json"), &v.to_string()).unwrap_err();
            assert!(e.msg.contains(want), "{e}");
        };
        bad(
            on("project", "0003_z", "database", None, "z"),
            "names the configured database",
        );
        let mut tenant_db = file("project", "0003_z", &[], json!([]));
        tenant_db["database"] = json!("one");
        bad(tenant_db, "belongs on a file with `target: \"database\"`");
        let mut req = on("project", "0003_z", "every_database", None, "z");
        req["requires"] = json!(["registry:0001_a"]);
        bad(req, "has no `requires`");
        let mut v2 = on("x", "0002_u", "database", Some("two"), "u");
        v2["parents"] = json!(["0001_t"]);
        write(&x, "0002_u", &v2);
        let e = load_project(&dir).unwrap_err();
        assert!(e.msg.contains("one database per app"), "{e}");
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
