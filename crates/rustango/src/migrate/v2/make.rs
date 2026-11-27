//! makemigrations v2 (4.7): the models as state, the ops from the replayed
//! leaf to it, and a self-check that replaying the ops reaches it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use super::format::{Kind, Op};
use super::render::dialect;
use super::replay::Ctx;
use super::state::{
    pg_object_name, Backend, CheckBody, CheckDef, ColumnDef, ExternColumn, ExternOwner,
    ExternTable, FkDef, IndexDef, OnDelete, State, StateDoc, TableState, Target,
};
use super::{Code, LoadError};
use crate::migrate::snapshot::{FieldSnapshot, SchemaSnapshot, TableSnapshot};

/// The prefix of the framework's own tables: external to project chains.
pub(crate) const FRAMEWORK_PREFIX: &str = "rustango_";

fn err(code: Code, at: impl Into<String>, msg: impl Into<String>) -> LoadError {
    LoadError::new(code, at, msg)
}

/// What `make` writes for.
#[derive(Debug, Clone, Default)]
pub(crate) struct MakeOptions {
    /// The configured backends; every default must be one string on all.
    pub(crate) backends: Vec<Backend>,
    /// The slug after the number; derived from the ops when absent.
    pub(crate) name: Option<String>,
    /// `table.column` → constant written into existing rows by `add_column`.
    pub(crate) fills: BTreeMap<String, String>,
    /// Plan only: nothing is written (`make --check`).
    pub(crate) dry_run: bool,
    /// `--rename table.old:new`: `(table, old, new)`.
    pub(crate) renames: Vec<(String, String, String)>,
    /// `--no-rename`: a drop and an add of one type are what they say.
    pub(crate) no_rename: bool,
    /// `--tenancy`: tenant models live apart from registry models, so an FK
    /// between them is refused (decision 4). Also on when the project has
    /// registry-target files.
    pub(crate) tenancy: bool,
    /// `--no-split`: contract ops stay in the one file (6.8).
    pub(crate) no_split: bool,
}

/// A contract op (6.8): a drop, a rename, or NOT NULL on an existing column.
pub(crate) fn is_contract_op(op: &Op) -> bool {
    match op {
        Op::DropTable { .. }
        | Op::DropColumn { .. }
        | Op::DropIndex { .. }
        | Op::DropCheck { .. }
        | Op::DropFk { .. }
        | Op::RenameTable { .. }
        | Op::RenameColumn { .. } => true,
        Op::AlterColumn { changes, .. } => {
            changes.nullable.as_ref().is_some_and(|n| n.from && !n.to)
        }
        _ => false,
    }
}

/// The file `make` wrote (or would write), with one line per op.
#[derive(Debug)]
pub(crate) struct Made {
    pub(crate) path: PathBuf,
    pub(crate) lines: Vec<String>,
}

/// Write the next v2 file of the app folder `folder` in the project around
/// `dir`, from `models`. `Ok(None)` when the models match the chain.
///
/// # Errors
/// Load and replay errors, two leaves, a default with no one spelling on
/// the configured backends, a NOT NULL column with no default or fill, an
/// op v2 does not generate yet, or an internal mismatch of the self-check.
/// A tenant model with an FK to a registry model: refused, as replay
/// refuses it (decision 4, 3.2), in both tenancy modes.
fn refuse_registry_fks(models: &SchemaSnapshot, at: &str) -> Result<(), LoadError> {
    use crate::core::ModelScope;
    let scope_of = |table: &str| {
        crate::core::ModelEntry::for_table(table).map_or(ModelScope::Tenant, |e| e.schema.scope)
    };
    for t in models
        .tables
        .iter()
        .filter(|t| scope_of(&t.name) == ModelScope::Tenant)
    {
        for f in &t.fields {
            let Some(fk) = &f.fk else { continue };
            if scope_of(&fk.to) == ModelScope::Registry {
                return Err(LoadError::new(
                    Code::CrossTarget,
                    at,
                    format!(
                        "tenant table `{}.{}` references registry table `{}`. Tenant→registry \
                         FKs are not allowed in v2: in DB mode the tables live in different \
                         databases, and schema mode must behave the same. Keep the column plain \
                         and check it in app code.",
                        t.name, f.column, fk.to
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn make(
    dir: &Path,
    folder: &Path,
    models: &SchemaSnapshot,
    opts: &MakeOptions,
) -> Result<Option<Made>, LoadError> {
    let project = super::load::load_project(dir)?;
    if opts.tenancy || project.order.contains_key(&Target::Registry) {
        refuse_registry_fks(models, &folder.display().to_string())?;
    }
    let replayed = super::load::replay(&project)?;
    let old = replayed
        .states
        .get(&Target::Tenant)
        .cloned()
        .unwrap_or_default();
    let app = super::adopt::app_label(crate::migrate::file::project_root_of(dir), folder);
    let mine: Vec<&super::load::Loaded> =
        project
            .order
            .get(&Target::Tenant)
            .map_or_else(Vec::new, |idx| {
                idx.iter()
                    .map(|&i| &project.files[i])
                    .filter(|f| f.file.app == app)
                    .collect()
            });
    // One leaf per app: the loader refuses two.
    let leaves: Vec<&str> = mine
        .last()
        .map(|f| f.file.id.as_str())
        .into_iter()
        .collect();
    let owned = owned_tables(&mine);
    let at = folder.display().to_string();
    let backends = if opts.backends.is_empty() {
        vec![Backend::Postgres]
    } else {
        opts.backends.clone()
    };
    let new = state_of(models, &backends, &at)?;
    let others = other_apps(&project, &app);
    let plan = diff(&old, &owned, new, &others, opts, &at)?;
    if plan.ops.is_empty() {
        return Ok(None);
    }
    let next = mine
        .iter()
        .filter_map(|f| crate::migrate::file::extract_index(&f.file.id))
        .max()
        .map_or(1, |n| n + 1);
    let slug = opts
        .name
        .clone()
        .unwrap_or_else(|| slug_of(&plan, old.tables.is_empty()));
    let id = format!("{next:04}_{slug}");
    let path = folder.join(format!("{id}.json"));
    let file = |id: &str, parents: Vec<String>, ops: &[Value], contract: bool| {
        let mut body = json!({
            "engine": 2, "format": 1, "id": id, "app": app, "target": "tenant",
            "parents": parents,
            "operations": ops,
        });
        if contract {
            body["phase"] = json!("contract");
        }
        format!("{}\n", serde_json::to_string_pretty(&body).expect("json"))
    };
    let leaves: Vec<String> = leaves.iter().map(|s| (*s).to_owned()).collect();
    let with_deps = |text: String| -> String {
        if plan.depends_on.is_empty() {
            return text;
        }
        let mut v: Value = serde_json::from_str(&text).expect("json");
        v["depends_on"] = json!(plan.depends_on);
        format!("{}\n", serde_json::to_string_pretty(&v).expect("json"))
    };
    let decoded: Vec<Option<Op>> = plan
        .ops
        .iter()
        .enumerate()
        .map(|(i, v)| decode_one(v, &format!("{at}#/operations/{i}")).ok())
        .collect();
    let contract: Vec<bool> = decoded
        .iter()
        .map(|op| op.as_ref().is_some_and(is_contract_op))
        .collect();
    // An expand op moves ahead of the contract ops before it only when it
    // commutes with each of them (4.5).
    let movable = decoded.iter().enumerate().all(|(j, e)| {
        if contract[j] {
            return true;
        }
        let Some(ek) = e.as_ref().and_then(|e| super::commute::keys(e, &at).ok()) else {
            return false;
        };
        decoded[..j].iter().zip(&contract).all(|(c, is_c)| {
            !is_c
                || c.as_ref()
                    .and_then(|c| super::commute::keys(c, &at).ok())
                    .is_some_and(|ck| ck.conflict(&ek).is_none())
        })
    });
    let any_contract = contract.iter().any(|c| *c);
    let mut lines = plan.lines.clone();
    // Expand ops in one file, contract ops in the next (6.8), when that
    // split replays to the models; else one file.
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    if !opts.no_split && movable && any_contract && contract.iter().any(|c| !*c) {
        let mut exp: Vec<Value> = Vec::new();
        let mut con: Vec<Value> = Vec::new();
        for (op, c) in plan.ops.iter().zip(&contract) {
            if *c {
                con.push(op.clone());
            } else {
                exp.push(op.clone());
            }
        }
        let id2 = format!("{:04}_{slug}_contract", next + 1);
        let path2 = folder.join(format!("{id2}.json"));
        let t1 = with_deps(file(&id, leaves.clone(), &exp, false));
        let t2 = file(&id2, vec![id.clone()], &con, true);
        let split_ok = super::load::load_file(&path, &t1).ok().and_then(|l| {
            let mut mid = old.clone();
            let ctx = Ctx {
                target: Target::Tenant,
                registry: None,
            };
            super::replay::apply_file(&mut mid, &l.file, &l.at, &ctx).ok()?;
            verify(&mid, &owned, &plan.new, &path2, &t2).ok()
        });
        if split_ok.is_some() {
            lines.push(format!("contract ops split into {}", path2.display()));
            files.push((path.clone(), t1));
            files.push((path2, t2));
        }
    }
    if files.is_empty() {
        let text = with_deps(file(&id, leaves, &plan.ops, any_contract));
        verify(&old, &owned, &plan.new, &path, &text)?;
        files.push((path.clone(), text));
    }
    if !opts.dry_run {
        use std::io::Write as _;
        std::fs::create_dir_all(folder).map_err(|e| err(Code::Io, &at, e.to_string()))?;
        for (p, text) in &files {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .map_err(|e| err(Code::Io, p.display().to_string(), e.to_string()))?;
            f.write_all(text.as_bytes())
                .map_err(|e| err(Code::Io, p.display().to_string(), e.to_string()))?;
        }
    }
    Ok(Some(Made { path, lines }))
}

/// One op of a file being written, decoded on its own.
fn decode_one(v: &Value, at: &str) -> Result<Op, LoadError> {
    let f = super::format::decode(
        &json!({"engine": 2, "format": 1, "id": "0001_x", "app": "x", "target": "tenant",
                "operations": [v]}),
        at,
    )?;
    Ok(f.operations.into_iter().next().expect("one op").op)
}

/// `make --data-rust <name>`: a file with one `data_rust` op (footprint
/// `read`/`write`), the step's `.rs` stub with a test, and its `mod` line.
///
/// # Errors
/// Load errors, a footprint table not in state, or a file that exists.
pub(crate) fn make_data_rust(
    dir: &Path,
    folder: &Path,
    name: &str,
    read: &[String],
    write: &[String],
) -> Result<Made, LoadError> {
    let project = super::load::load_project(dir)?;
    let root = crate::migrate::file::project_root_of(dir);
    let app = super::adopt::app_label(root, folder);
    let mine: Vec<&super::load::Loaded> = project
        .order
        .get(&Target::Tenant)
        .map(|idx| {
            idx.iter()
                .map(|&i| &project.files[i])
                .filter(|f| f.file.app == app)
                .collect()
        })
        .unwrap_or_default();
    let next = mine
        .iter()
        .filter_map(|f| crate::migrate::file::extract_index(&f.file.id))
        .max()
        .map_or(1, |n| n + 1);
    let id = format!("{next:04}_{name}");
    let step = format!("{name}_v1");
    let path = folder.join(format!("{id}.json"));
    let body = json!({
        "engine": 2, "format": 1, "id": id, "app": app, "target": "tenant",
        "parents": mine.last().map(|f| vec![f.file.id.clone()]).unwrap_or_default(),
        "operations": [{"op": "data_rust", "v": 1, "id": step,
            "tables": {"read": read, "write": write}, "down": "rust"}],
    });
    let text = format!("{}\n", serde_json::to_string_pretty(&body).expect("json"));
    // The op must replay: its footprint names tables in state.
    let loaded = super::load::load_file(&path, &text)?;
    let mut after = super::load::replay(&project)?
        .states
        .remove(&Target::Tenant)
        .unwrap_or_default();
    super::replay::apply_file(
        &mut after,
        &loaded.file,
        &loaded.at,
        &Ctx {
            target: Target::Tenant,
            registry: None,
        },
    )?;
    let camel: String = name
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect();
    let steps = root.join("data_steps");
    let rs = steps.join(format!("{name}.rs"));
    let stub = format!(
        r#"//! Data step `{app}:{step}`, run by migration `{id}`.
use rustango::migrate::v2::{{Batch, StepCtx, StepResult}};

#[rustango::data_step(app = "{app}", id = "{step}")]
impl {camel} {{
    async fn up(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {{
        // Read and write through `ctx.table("...")`: only the op's `tables`.
        let _ = ctx;
        Ok(batch.done())
    }}

    async fn down(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {{
        let _ = ctx;
        Ok(batch.done())
    }}
}}

#[cfg(test)]
mod tests {{
    /// Point `DATABASE_URL` at an empty test database.
    #[tokio::test]
    async fn down_restores_what_up_changed() {{
        let Ok(url) = std::env::var("DATABASE_URL") else {{ return }};
        let pool = rustango::sql::Pool::connect(&url).await.unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("{migrations}");
        let step = rustango::migrate::v2::testing::at(&pool, &dir, "{app}", "{step}")
            .await
            .unwrap();
        step.assert_prefix_down().await.unwrap();
    }}
}}
"#,
        migrations = dir
            .strip_prefix(root.parent().unwrap_or(root))
            .unwrap_or(dir)
            .display(),
    );
    let at = path.display().to_string();
    let io = |e: std::io::Error| err(Code::Io, &at, e.to_string());
    std::fs::create_dir_all(&steps).map_err(io)?;
    for (p, t) in [(&path, text.as_str()), (&rs, stub.as_str())] {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
            .and_then(|mut f| f.write_all(t.as_bytes()))
            .map_err(io)?;
    }
    let mod_rs = steps.join("mod.rs");
    let line = format!("pub mod {name};\n");
    let mut mods = std::fs::read_to_string(&mod_rs).unwrap_or_default();
    if !mods.contains(&line) {
        mods.push_str(&line);
        std::fs::write(&mod_rs, mods).map_err(io)?;
    }
    Ok(Made {
        path,
        lines: vec![
            format!("+ data_rust {app}:{step}"),
            format!("wrote {}", rs.display()),
            format!("added `pub mod {name};` to {}", mod_rs.display()),
            "add `mod data_steps;` to your crate root once".to_owned(),
        ],
    })
}

/// Tables other apps own, each mapped to `<app>:<leaf>` for `depends_on`.
fn other_apps(project: &super::load::Project, app: &str) -> BTreeMap<String, String> {
    let Some(idx) = project.order.get(&Target::Tenant) else {
        return BTreeMap::new();
    };
    let mut by_app: BTreeMap<&str, Vec<&super::load::Loaded>> = BTreeMap::new();
    for &i in idx {
        let f = &project.files[i];
        if f.file.app != app {
            by_app.entry(f.file.app.as_str()).or_default().push(f);
        }
    }
    let mut out = BTreeMap::new();
    for (other, files) in by_app {
        // Canonical order: the last file is a leaf (one per app, checked at load).
        let leaf = files.last().map(|f| f.file.id.clone()).unwrap_or_default();
        for t in owned_tables(&files) {
            out.insert(t, format!("{other}:{leaf}"));
        }
    }
    out
}

/// Tables the app's files own at its leaf, in canonical order.
fn owned_tables(mine: &[&super::load::Loaded]) -> BTreeSet<String> {
    use super::format::Op;
    let mut out = BTreeSet::new();
    for f in mine {
        if f.file.kind == Kind::Checkpoint {
            if let Some(doc) = &f.file.state {
                out.extend(doc.tables.keys().cloned());
            }
        }
        for env in &f.file.operations {
            match &env.op {
                Op::CreateTable { table, .. } | Op::AdoptTable { table, .. } => {
                    out.insert(table.name.clone());
                }
                Op::DropTable { table, .. } => {
                    out.remove(table);
                }
                Op::RenameTable { from, to } => {
                    out.remove(from);
                    out.insert(to.clone());
                }
                _ => {}
            }
        }
    }
    out
}

/// Replay `text` on `old` and compare every table the app owns, before or
/// after, with `new` (4.7 step 3).
fn verify(
    old: &State,
    owned: &BTreeSet<String>,
    new: &StateDoc,
    path: &Path,
    text: &str,
) -> Result<(), LoadError> {
    let loaded = super::load::load_file(path, text)?;
    let mut after = old.clone();
    let ctx = Ctx {
        target: Target::Tenant,
        registry: None,
    };
    super::replay::apply_file(&mut after, &loaded.file, &loaded.at, &ctx)?;
    let names: BTreeSet<&String> = owned.iter().chain(new.tables.keys()).collect();
    for name in names {
        let got = after.tables.get(name.as_str()).map(|t| t.normalised());
        let want = new.tables.get(name).map(TableState::normalised);
        if got != want {
            return Err(err(
                Code::State,
                &loaded.at,
                format!(
                    "internal: the ops do not reach the models for `{name}`: replayed {got:?}, \
                     models {want:?}"
                ),
            ));
        }
    }
    for (name, e) in &new.externs {
        if after.externs.get(name.as_str()).map(|x| x.normalised()) != Some(e.normalised()) {
            return Err(err(
                Code::State,
                &loaded.at,
                format!("internal: extern `{name}` does not match the models"),
            ));
        }
    }
    Ok(())
}

/// The models as v2 state. Defaults are the one string every backend in
/// `backends` takes; min/max become checks. FK covering indexes are added
/// by the differ, which knows the chain.
///
/// # Errors
/// M2M tables, exclusions and composite FKs (a later release), and a
/// default with no one spelling on `backends`.
pub(crate) fn state_of(
    snap: &SchemaSnapshot,
    backends: &[Backend],
    at: &str,
) -> Result<StateDoc, LoadError> {
    if !snap.m2m_tables.is_empty() || !snap.excludes.is_empty() {
        return Err(err(
            Code::NotEnabled,
            at,
            "many-to-many tables and exclusion constraints come in a later release",
        ));
    }
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
                format!("table `{}`: composite FKs come in a later release", t.name),
            ));
        }
        let mut state = table_of(t, snap, backends, at)?;
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

pub(crate) fn table_of(
    t: &TableSnapshot,
    snap: &SchemaSnapshot,
    backends: &[Backend],
    at: &str,
) -> Result<TableState, LoadError> {
    let mut columns = Vec::new();
    let mut indexes = Vec::new();
    let mut checks = Vec::new();
    let mut fks = Vec::new();
    for f in &t.fields {
        columns.push(column_of(&t.name, f, backends, at)?);
        if f.unique && !f.primary_key {
            // The name engine v1 gives an inline UNIQUE, on every backend.
            indexes.push(IndexDef {
                name: crate::migrate::ddl::unique_constraint_name(&t.name, &f.column),
                columns: vec![f.column.clone()],
                unique: true,
                method: "btree".into(),
                predicate: None,
                include: Vec::new(),
                skipped_on: Vec::new(),
                constraint: true,
            });
        }
        if f.min.is_some() || f.max.is_some() {
            // A range: each backend quotes the column its own way.
            checks.push(CheckDef {
                name: pg_object_name(&t.name, Some(&f.column), "check", |_| false),
                body: CheckBody::Range {
                    column: f.column.clone(),
                    min: f.min,
                    max: f.max,
                },
            });
        }
        if let Some(rel) = &f.fk {
            fks.push(FkDef {
                name: crate::migrate::ddl::fk_constraint_name(&t.name, &f.column),
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
            constraint: false,
        });
    }
    for c in snap.checks.iter().filter(|c| c.table == t.name) {
        checks.push(CheckDef {
            name: c.name.clone(),
            body: CheckBody::Sql(c.expr.clone()),
        });
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

fn column_of(
    table: &str,
    f: &FieldSnapshot,
    backends: &[Backend],
    at: &str,
) -> Result<ColumnDef, LoadError> {
    let portable_default = f
        .default
        .as_deref()
        .and_then(|e| super::state::PortableDefault::of(e, &f.ty));
    let default = match f.default.as_deref() {
        None => None,
        Some(_) if portable_default.is_some() => None,
        Some(e) => Some(
            one_default(e, &f.ty, f.max_length, backends).map_err(|(a, b)| {
                err(
                    Code::Dialect,
                    at,
                    format!(
                    "`{table}.{}`: the default is `{}` on {} but `{}` on {}; one string must run \
                     on every configured backend: set this value in app code",
                    f.column,
                    a.1,
                    a.0.as_str(),
                    b.1,
                    b.0.as_str()
                ),
                )
            })?,
        ),
    };
    Ok(ColumnDef {
        column: f.column.clone(),
        ty: f.ty.clone(),
        nullable: f.nullable,
        max_length: f.max_length,
        primary_key: f.primary_key,
        auto: f.auto && f.primary_key,
        default,
        portable_default,
        generated_as: f.generated_as.clone(),
        comment: f.db_comment.clone(),
        case_insensitive: f.case_insensitive,
        sealed: false,
    })
}

type Spelling = (Backend, String);

/// The model default as SQL text for every backend, folded into one string
/// (2.6). Spellings equal but for outer parens fold to the parenthesised
/// one, which every backend takes. `Err` names two backends that differ.
fn one_default(
    expr: &str,
    ty: &str,
    max_length: Option<u32>,
    backends: &[Backend],
) -> Result<String, (Spelling, Spelling)> {
    let all: Vec<Spelling> = backends
        .iter()
        .map(|&b| {
            let text =
                crate::migrate::diff::render_column_default(expr, ty, max_length, dialect(b));
            (b, text)
        })
        .collect();
    let first = all.first().expect("at least one backend").clone();
    let bare = |s: &str| -> String {
        let t = s.trim();
        match t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
            Some(inner) if !inner.contains(['(', ')']) => inner.trim().to_owned(),
            _ => t.to_owned(),
        }
    };
    for s in &all[1..] {
        if bare(&s.1) != bare(&first.1) {
            return Err((first, s.clone()));
        }
    }
    let wrapped = all.iter().find(|s| s.1.trim().starts_with('('));
    Ok(match wrapped {
        Some(w) if all.iter().any(|s| s.1 != w.1) => w.1.clone(),
        _ => first.1,
    })
}

/// The ops of one file, the lines `make` prints, and the models state with
/// the covering indexes the chain needs.
struct Plan {
    ops: Vec<Value>,
    lines: Vec<String>,
    depends_on: Vec<String>,
    new: StateDoc,
}

fn op(kind: &str, mut payload: Value) -> Value {
    let obj = payload.as_object_mut().expect("object");
    obj.insert("op".into(), json!(kind));
    obj.insert("v".into(), json!(1));
    payload
}

pub(crate) fn field_json(c: &ColumnDef, fk: Option<&FkDef>) -> Value {
    let mut v = serde_json::to_value(c).expect("column");
    if let Some(fk) = fk {
        v["fk"] = inline_fk(fk);
    }
    v
}

pub(crate) fn inline_fk(fk: &FkDef) -> Value {
    let mut v = json!({"name": fk.name, "to": fk.to, "on": fk.on[0]});
    if fk.on_delete != OnDelete::NoAction {
        v["on_delete"] = serde_json::to_value(fk.on_delete).expect("on_delete");
    }
    if let Some(t) = &fk.to_ty {
        v["to_ty"] = json!(t);
    }
    v
}

/// The ops from `old` to `new`, ordered so each replays (4.7 step 2).
#[allow(clippy::too_many_lines)]
fn diff(
    old: &State,
    owned: &BTreeSet<String>,
    mut new: StateDoc,
    others: &BTreeMap<String, String>,
    opts: &MakeOptions,
    at: &str,
) -> Result<Plan, LoadError> {
    let fills = &opts.fills;
    let empty = TableState {
        name: String::new(),
        scope: Target::Tenant,
        columns: Vec::new(),
        indexes: Vec::new(),
        checks: Vec::new(),
        fks: Vec::new(),
        comment: None,
        pg_pkey: None,
        pg_sequence: None,
    };
    let old_of = |name: &str| -> Option<&Arc<TableState>> {
        owned.contains(name).then(|| old.tables.get(name)).flatten()
    };
    // Covering indexes: an unchanged FK keeps what the chain has; a new one
    // gets `<t>_<c>_idx` when nothing covers it.
    for t in new.tables.values_mut() {
        let before = old_of(&t.name);
        for fk in t.fks.clone() {
            if t.indexes
                .iter()
                .any(|i| i.columns.first() == fk.columns.first())
                || t.primary_key().next() == fk.columns.first().map(String::as_str)
            {
                continue;
            }
            let unchanged = before.and_then(|b| {
                b.fks
                    .iter()
                    .find(|f| f.name == fk.name && f.columns == fk.columns)
            });
            let ix = match (unchanged, before) {
                // An FK the chain has keeps what covers it there, if anything
                // does (an adopted PG FK may have no index).
                (Some(_), Some(b)) => match b
                    .indexes
                    .iter()
                    .find(|i| i.columns.first() == fk.columns.first())
                {
                    Some(ix) => ix.clone(),
                    None => continue,
                },
                _ => IndexDef {
                    name: pg_object_name(&t.name, Some(&fk.columns[0]), "idx", |_| false),
                    columns: fk.columns.clone(),
                    unique: false,
                    method: "btree".into(),
                    predicate: None,
                    include: Vec::new(),
                    skipped_on: Vec::new(),
                    constraint: false,
                },
            };
            t.indexes.push(ix);
        }
        t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    }

    let mut externs = Vec::new();
    let mut drop_fks = Vec::new();
    let mut drop_checks = Vec::new();
    let mut drop_indexes = Vec::new();
    let mut drop_columns = Vec::new();
    let mut drop_tables = Vec::new();
    let mut rename_columns = Vec::new();
    let mut creates = Vec::new();
    let mut add_columns = Vec::new();
    let mut alters = Vec::new();
    let mut comments = Vec::new();
    let mut add_indexes = Vec::new();
    let mut add_checks = Vec::new();
    let mut add_fks = Vec::new();
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut depends_on = BTreeSet::new();

    for (name, e) in &new.externs {
        if old.externs.get(name.as_str()).map(|x| x.normalised()) != Some(e.normalised()) {
            externs.push(op("extern_table", serde_json::to_value(e).expect("extern")));
            lines.push((0, format!("+ extern_table {name}")));
        }
    }

    // Columns whose type changes: FKs on either side are dropped and re-added (3780).
    let mut retyped: BTreeSet<(String, String)> = BTreeSet::new();
    for (name, t) in &new.tables {
        if let Some(b) = old_of(name) {
            for c in &t.columns {
                if b.column(&c.column).is_some_and(|o| o.ty != c.ty) {
                    retyped.insert((name.clone(), c.column.clone()));
                }
            }
        }
    }
    let touches = |table: &str, fk: &FkDef| {
        fk.columns
            .iter()
            .any(|c| retyped.contains(&(table.to_owned(), c.clone())))
            || fk
                .on
                .iter()
                .any(|c| retyped.contains(&(fk.to.clone(), c.clone())))
    };

    // Tables only in old.
    for name in owned {
        if new.tables.contains_key(name) {
            continue;
        }
        let Some(b) = old.tables.get(name.as_str()) else {
            continue;
        };
        drop_tables.push(op(
            "drop_table",
            json!({"table": name, "was": serde_json::to_value(&**b).expect("table")}),
        ));
        lines.push((4, format!("- drop_table {name}")));
    }
    // FKs from surviving tables into dropped ones go first.
    for (name, t) in &new.tables {
        let b = old_of(name).map_or(&empty, |b| &**b);
        let mine: Vec<&FkDef> = t.fks.iter().collect();
        for f in &b.fks {
            let same = mine.iter().any(|n| *n == f);
            if !same || touches(name, f) {
                drop_fks.push(op(
                    "drop_fk",
                    json!({"table": name, "column": f.columns[0], "was": inline_fk(f)}),
                ));
                lines.push((1, format!("- drop_fk {name}.{}", f.name)));
            }
        }
    }

    // Tables to create, in FK order; an FK into a table created later, or
    // into itself, becomes an add_fk after.
    let mut pending: Vec<&TableState> = new
        .tables
        .values()
        .filter(|t| old_of(&t.name).is_none())
        .collect();
    let mut exists: BTreeSet<String> = old.tables.keys().map(ToString::to_string).collect();
    while !pending.is_empty() {
        let pos = pending
            .iter()
            .position(|t| {
                t.fks.iter().all(|f| {
                    f.to == t.name || exists.contains(&f.to) || new.externs.contains_key(&f.to)
                })
            })
            .unwrap_or(0);
        let t = pending.remove(pos);
        let mut fields = Vec::new();
        for c in &t.columns {
            let fk = t.fks.iter().find(|f| {
                f.columns.len() == 1
                    && f.columns[0] == c.column
                    && f.to != t.name
                    && (exists.contains(&f.to) || new.externs.contains_key(&f.to))
            });
            fields.push(field_json(c, fk));
        }
        for f in &t.fks {
            let inline =
                f.to != t.name && (exists.contains(&f.to) || new.externs.contains_key(&f.to));
            if !inline {
                add_fks.push(op(
                    "add_fk",
                    json!({"table": t.name, "column": f.columns[0], "fk": inline_fk(f)}),
                ));
                lines.push((11, format!("+ add_fk {}.{}", t.name, f.name)));
            }
            if !owned.contains(&f.to) && !new.tables.contains_key(&f.to) {
                if let Some(dep) = others.get(&f.to) {
                    depends_on.insert(dep.clone());
                }
            }
        }
        let mut table = json!({"name": t.name, "scope": "tenant", "fields": fields});
        if let Some(c) = &t.comment {
            table["comment"] = json!(c);
        }
        let mut payload = json!({"table": table});
        if !t.indexes.is_empty() {
            payload["indexes"] = serde_json::to_value(&t.indexes).expect("indexes");
        }
        if !t.checks.is_empty() {
            payload["checks"] = serde_json::to_value(&t.checks).expect("checks");
        }
        creates.push(op("create_table", payload));
        lines.push((5, format!("+ create_table {}", t.name)));
        exists.insert(t.name.clone());
    }

    // Tables in both.
    for (name, t) in &new.tables {
        let Some(b) = old_of(name) else { continue };
        // `new column -> old column` of each rename asked for on this table.
        let mut renamed: BTreeMap<&str, &str> = BTreeMap::new();
        for (table, from, to) in opts.renames.iter().filter(|r| &r.0 == name) {
            if b.column(from).is_none() || t.column(to).is_none() || b.column(to).is_some() {
                return Err(err(
                    Code::Rename,
                    at,
                    format!(
                        "`--rename {table}.{from}:{to}`: `{from}` must be in the chain and `{to}` \
                         only in the models"
                    ),
                ));
            }
            renamed.insert(to, from);
        }
        let dropped: Vec<&ColumnDef> = b
            .columns
            .iter()
            .filter(|c| t.column(&c.column).is_none() && !renamed.values().any(|f| *f == c.column))
            .collect();
        let added: Vec<&ColumnDef> = t
            .columns
            .iter()
            .filter(|c| b.column(&c.column).is_none() && !renamed.contains_key(c.column.as_str()))
            .collect();
        if !opts.no_rename {
            if let Some((d, a)) = dropped
                .iter()
                .find_map(|d| added.iter().find(|a| a.ty == d.ty).map(|a| (d, a)))
            {
                return Err(err(
                    Code::Rename,
                    at,
                    format!(
                        "`{name}`: `{}` goes and `{}` comes, both `{}`. A rename keeps the data: \
                         give `--rename {name}.{}:{}`, or `--no-rename` to drop and add",
                        d.column, a.column, a.ty, d.column, a.column
                    ),
                ));
            }
        }
        for c in dropped {
            drop_columns.push(op(
                "drop_column",
                json!({"table": name, "column": c.column, "was": field_json(c, None)}),
            ));
            lines.push((3, format!("- drop_column {name}.{}", c.column)));
        }
        for (to, from) in &renamed {
            rename_columns.push(op(
                "rename_column",
                json!({"table": name, "from": from, "to": to}),
            ));
            lines.push((3, format!("-> rename_column {name}.{from} -> {to}")));
        }
        for c in &t.columns {
            let before = match renamed.get(c.column.as_str()) {
                Some(from) => b.column(from).map(|o| ColumnDef {
                    column: c.column.clone(),
                    ..o.clone()
                }),
                None => b.column(&c.column).cloned(),
            };
            match before {
                None => {
                    let key = format!("{name}.{}", c.column);
                    let mut payload = json!({"table": name, "column": field_json(c, None)});
                    if let Some(fill) = fills.get(&key) {
                        payload["fill"] = json!(fill);
                    } else if !c.nullable && !c.has_default() && c.generated_as.is_none() {
                        return Err(err(
                            Code::Fill,
                            at,
                            format!(
                                "`{key}` is NOT NULL with no default on an existing table: give \
                                 `--fill {key}=<sql>` or a default"
                            ),
                        ));
                    }
                    add_columns.push(op("add_column", payload));
                    lines.push((6, format!("+ add_column {key}")));
                }
                Some(o) if &o != c => {
                    alters.push(op(
                        "alter_column",
                        json!({"table": name, "column": c.column, "changes": changes(&o, c)}),
                    ));
                    lines.push((7, format!("~ alter_column {name}.{}", c.column)));
                }
                Some(_) => {}
            }
        }
        if b.comment != t.comment {
            comments.push(op(
                "alter_table_comment",
                json!({"table": name, "from": b.comment, "to": t.comment}),
            ));
            lines.push((8, format!("~ alter_table_comment {name}")));
        }
        for i in &b.indexes {
            if !t.indexes.iter().any(|n| n.normalised() == i.normalised()) {
                drop_indexes.push(op(
                    "drop_index",
                    json!({"table": name, "name": i.name, "was": serde_json::to_value(i).expect("index")}),
                ));
                lines.push((2, format!("- drop_index {name}.{}", i.name)));
            }
        }
        for i in &t.indexes {
            if !b.indexes.iter().any(|o| o.normalised() == i.normalised()) {
                let mut payload = serde_json::to_value(i).expect("index");
                payload["table"] = json!(name);
                add_indexes.push(op("add_index", payload));
                lines.push((9, format!("+ add_index {name}.{}", i.name)));
            }
        }
        for c in &b.checks {
            if !t.checks.contains(c) {
                drop_checks.push(op(
                    "drop_check",
                    json!({"table": name, "name": c.name, "was": c.body}),
                ));
                lines.push((1, format!("- drop_check {name}.{}", c.name)));
            }
        }
        for c in &t.checks {
            if !b.checks.contains(c) {
                let mut payload = serde_json::to_value(c).expect("check");
                payload["table"] = json!(name);
                add_checks.push(op("add_check", payload));
                lines.push((10, format!("+ add_check {name}.{}", c.name)));
            }
        }
        for f in &t.fks {
            if !b.fks.contains(f) || touches(name, f) {
                add_fks.push(op(
                    "add_fk",
                    json!({"table": name, "column": f.columns[0], "fk": inline_fk(f)}),
                ));
                lines.push((11, format!("+ add_fk {name}.{}", f.name)));
                if !owned.contains(&f.to) && !new.tables.contains_key(&f.to) {
                    if let Some(dep) = others.get(&f.to) {
                        depends_on.insert(dep.clone());
                    }
                }
            }
        }
    }
    // Drop dependants first: tables that others pointed at go last.
    drop_tables.reverse();
    let ops: Vec<Value> = [
        externs,
        drop_fks,
        drop_checks,
        drop_indexes,
        drop_columns,
        drop_tables,
        rename_columns,
        creates,
        add_columns,
        alters,
        comments,
        add_indexes,
        add_checks,
        add_fks,
    ]
    .into_iter()
    .flatten()
    .collect();
    lines.sort_by_key(|(k, _)| *k);
    Ok(Plan {
        ops,
        lines: lines.into_iter().map(|(_, l)| l).collect(),
        depends_on: depends_on.into_iter().collect(),
        new,
    })
}

fn changes(o: &ColumnDef, c: &ColumnDef) -> Value {
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, from: Value, to: Value| {
        if from != to {
            m.insert(k.into(), json!({"from": from, "to": to}));
        }
    };
    put("ty", json!(o.ty), json!(c.ty));
    put("nullable", json!(o.nullable), json!(c.nullable));
    put("default", json!(o.default), json!(c.default));
    put(
        "portable_default",
        json!(o.portable_default),
        json!(c.portable_default),
    );
    put("max_length", json!(o.max_length), json!(c.max_length));
    put("comment", json!(o.comment), json!(c.comment));
    put(
        "case_insensitive",
        json!(o.case_insensitive),
        json!(c.case_insensitive),
    );
    put("primary_key", json!(o.primary_key), json!(c.primary_key));
    put("auto", json!(o.auto), json!(c.auto));
    put("generated_as", json!(o.generated_as), json!(c.generated_as));
    put("sealed", json!(o.sealed), json!(c.sealed));
    Value::Object(m)
}

/// `create_<t>` for one new table, `initial` for a first file, else `changes`.
fn slug_of(plan: &Plan, first: bool) -> String {
    let creates: Vec<&str> = plan
        .ops
        .iter()
        .filter(|o| o["op"] == "create_table")
        .filter_map(|o| o["table"]["name"].as_str())
        .collect();
    let raw = if first {
        "initial".to_owned()
    } else if plan.ops.len() == 1 {
        let o = &plan.ops[0];
        let kind = o["op"].as_str().unwrap_or("change");
        let what = o["table"]["name"]
            .as_str()
            .or_else(|| o["table"].as_str())
            .unwrap_or("");
        let col = o["column"]["column"]
            .as_str()
            .or_else(|| o["column"].as_str())
            .map(|c| format!("_{c}"))
            .unwrap_or_default();
        format!("{kind}_{what}{col}")
    } else if creates.len() == plan.ops.len() && creates.len() == 1 {
        format!("create_{}", creates[0])
    } else {
        "changes".to_owned()
    };
    let mut s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    s.truncate(50);
    s
}

#[cfg(test)]
#[path = "make_tests.rs"]
mod tests;
