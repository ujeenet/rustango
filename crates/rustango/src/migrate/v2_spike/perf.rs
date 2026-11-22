//! SPIKE S2: replay and plan performance for migrations engine v2.
//! A synthetic chain generator plus the stages a real loader runs (parse,
//! checksum, decode, DAG, replay, plan), each separately timeable.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::ops::{CheckDecl, ColumnDecl, FkDecl, IndexDecl, TableDecl};
use super::render::{apply, render_file, IndexState, Op, State, TableState};
use crate::sql::Dialect;

// -------------------------------------------------------------- generator

/// Chain shape: `apps` apps, `files_per_app` files each (the first is the
/// adopt checkpoint), `tables_per_app` tables (`adopt_tables` of them adopted).
#[derive(Clone, Copy)]
pub struct Shape {
    pub apps: usize,
    pub files_per_app: usize,
    pub tables_per_app: usize,
    pub adopt_tables: usize,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize
    }
}

fn col(name: &str, ty: &str, len: Option<u32>) -> Value {
    json!({"column": name, "ty": ty, "max_length": len, "nullable": true})
}

fn table_json(name: &str, fk_to: Option<&str>, n_cols: usize) -> Value {
    let mut fields = vec![json!({"column": "id", "ty": "i64", "primary_key": true,
        "auto": true, "nullable": false})];
    for c in 0..n_cols {
        fields.push(col(&format!("f{c}"), "string", Some(60)));
    }
    let mut fks = Vec::new();
    if let Some(p) = fk_to {
        fields.push(json!({"column": "parent_id", "ty": "i64", "nullable": true}));
        fks.push(
            json!({"name": format!("{name}_parent_fk"), "columns": ["parent_id"],
            "to": p, "ref_columns": ["id"], "on_delete": "CASCADE"}),
        );
    }
    json!({"name": name, "scope": "tenant", "fields": fields, "fks": fks,
        "checks": [{"name": format!("{name}_f0_ck"), "expr": "length(f0) > 0"}]})
}

fn indexes_json(name: &str, with_fk: bool) -> Vec<Value> {
    let mut v = vec![json!({"name": format!("{name}_f1_idx"), "table": name,
        "columns": ["f1"], "unique": false, "method": "btree"})];
    if with_fk {
        v.push(json!({"name": format!("{name}_parent_idx"), "table": name,
            "columns": ["parent_id"], "unique": false, "method": "btree"}));
    }
    v
}

fn file_json(
    app: &str,
    i: usize,
    adopt: bool,
    deps: &[String],
    ops: Vec<Value>,
) -> (String, Value) {
    let id = file_id(i);
    let parents: Vec<String> = if i == 0 { vec![] } else { vec![file_id(i - 1)] };
    let mut v = json!({"engine": 2, "format": 1, "kind": "migration", "id": id, "app": app,
        "parents": parents, "depends_on": deps, "target": "tenant", "phase": "expand",
        "created_at": "2026-11-22T00:00:00Z", "description": "synthetic", "operations": ops});
    if adopt {
        v["base"] = json!({"legacy": "0042_legacy", "state_sha256": "x"});
    }
    (id, v)
}

fn file_id(i: usize) -> String {
    format!("{:04}_step", i + 1)
}

/// The chain as JSON text, one `(app, text)` per file, in a shuffled-free
/// per-app order. Deterministic.
#[must_use]
pub fn generate(sh: Shape) -> Vec<String> {
    let mut rng = Rng(7);
    let mut out = Vec::new();
    for a in 0..sh.apps {
        let app = format!("a{a}");
        let tname = |t: usize| format!("{app}_t{t}");
        let dep = if a == 0 {
            vec![]
        } else {
            vec!["a0:0001_step".to_string()]
        };
        let mut tables: Vec<String> = Vec::new();
        // plain columns not used by an index or check: (table, col, width)
        let mut plain: Vec<(String, String, u32)> = Vec::new();
        for i in 0..sh.files_per_app {
            let mut ops = Vec::new();
            if i == 0 {
                for t in 0..sh.adopt_tables {
                    ops.push(
                        json!({"op": "adopt_table", "v": 1, "table": table_json(&tname(t), None, 4),
                        "indexes": indexes_json(&tname(t), false)}),
                    );
                    tables.push(tname(t));
                }
            } else if tables.len() < sh.tables_per_app {
                for _ in 0..5 {
                    if tables.len() >= sh.tables_per_app {
                        break;
                    }
                    let name = tname(tables.len());
                    // a1.. point at an a0 adopted table (cross-app edge); a0 at its own.
                    let parent = if a == 0 {
                        tables[rng.next() % tables.len()].clone()
                    } else {
                        format!("a0_t{}", rng.next() % sh.adopt_tables)
                    };
                    ops.push(json!({"op": "create_table", "v": 1,
                        "table": table_json(&name, Some(&parent), 4),
                        "indexes": indexes_json(&name, true)}));
                    tables.push(name);
                }
            } else {
                let t = tables[rng.next() % tables.len()].clone();
                let t2 = tables[rng.next() % tables.len()].clone();
                let x = format!("x{a}_{i}");
                let p = format!("p{a}_{i}");
                ops.push(json!({"op": "add_column", "v": 1, "table": t,
                    "column": col(&x, "string", Some(40))}));
                ops.push(
                    json!({"op": "add_index", "v": 1, "index": {"name": format!("ix_{a}_{i}"),
                    "table": t, "columns": [x], "unique": false, "method": "btree"}}),
                );
                ops.push(json!({"op": "add_check", "v": 1, "table": t,
                    "check": {"name": format!("ck_{a}_{i}"), "expr": format!("length({x}) > 0")}}));
                ops.push(json!({"op": "add_column", "v": 1, "table": t2,
                    "column": col(&p, "string", Some(40))}));
                plain.push((t2, p, 40));
                if plain.len() > 3 {
                    let (pt, pc, w) = plain.remove(0);
                    if i % 2 == 0 {
                        ops.push(json!({"op": "alter_column", "v": 1, "table": pt, "column": pc,
                            "from": col(&pc, "string", Some(w)), "to": col(&pc, "string", Some(80))}));
                        plain.push((pt, pc, 80));
                    } else {
                        ops.push(
                            json!({"op": "drop_column", "v": 1, "table": pt, "column": pc,
                            "was": col(&pc, "string", Some(w))}),
                        );
                    }
                }
            }
            let (_, v) = file_json(&app, i, i == 0, &dep, ops);
            out.push(serde_json::to_string_pretty(&v).expect("json"));
        }
    }
    out
}

// ------------------------------------------------------- parse / checksum

/// # Errors
/// Malformed JSON.
pub fn parse(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

const DENY: [&str; 2] = ["created_at", "description"];
const BATCH_KNOBS: [&str; 4] = ["size", "sleep_ms", "max_batches_per_run", "timeout_ms"];

fn canon(v: &Value, key: &str, out: &mut String) -> Result<(), String> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                return Err("floats are a load error".into());
            }
            out.push_str(&n.to_string());
        }
        Value::String(s) => out.push_str(&serde_json::to_string(s).map_err(|e| e.to_string())?),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canon(x, "", out)?;
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            let mut first = true;
            for k in keys {
                if k == "label" || (key == "batch" && BATCH_KNOBS.contains(&k.as_str())) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&serde_json::to_string(k).map_err(|e| e.to_string())?);
                out.push(':');
                canon(&m[k], k, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// SHA-256 of the canonical form, minus the deny-list.
///
/// # Errors
/// A float anywhere in the value.
pub fn checksum(v: &Value) -> Result<String, String> {
    let mut v = v.clone();
    if let Some(m) = v.as_object_mut() {
        for k in DENY {
            m.remove(k);
        }
    }
    let mut s = String::new();
    canon(&v, "", &mut s)?;
    Ok(hex(&Sha256::digest(s.as_bytes())))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------ decode

#[derive(Debug)]
pub struct LoadedFile {
    pub app: String,
    pub id: String,
    pub parents: Vec<String>,
    pub depends_on: Vec<String>,
    pub ops: Vec<Op>,
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str()
        .unwrap_or_else(|| panic!("missing `{k}`"))
        .to_string()
}
fn strs(v: &Value, k: &str) -> Vec<String> {
    v[k].as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().expect("str").to_string())
                .collect()
        })
        .unwrap_or_default()
}
fn column(v: &Value) -> ColumnDecl {
    let mut c = ColumnDecl::new(
        v["column"].as_str().expect("column"),
        v["ty"].as_str().expect("ty"),
    );
    c.max_length = v["max_length"].as_u64().map(|n| n as u32);
    c.nullable = v["nullable"].as_bool().unwrap_or(true);
    c.primary_key = v["primary_key"].as_bool().unwrap_or(false);
    c.auto = v["auto"].as_bool().unwrap_or(false);
    c.default = v["default"].as_str().map(str::to_string);
    c
}
fn index(v: &Value) -> IndexState {
    IndexState {
        decl: IndexDecl {
            name: s(v, "name"),
            table: s(v, "table"),
            columns: strs(v, "columns"),
            unique: v["unique"].as_bool().unwrap_or(false),
            method: s(v, "method"),
            predicate: None,
            include: vec![],
        },
        skipped_on: vec![],
    }
}
fn check(v: &Value) -> CheckDecl {
    CheckDecl {
        name: s(v, "name"),
        expr: s(v, "expr"),
    }
}
fn table_decl(t: &Value) -> TableDecl {
    TableDecl {
        name: s(t, "name"),
        columns: t["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .map(column)
            .collect(),
        comment: None,
        checks: t["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .map(check)
            .collect(),
        fks: t["fks"]
            .as_array()
            .expect("fks")
            .iter()
            .map(|f| FkDecl {
                name: s(f, "name"),
                columns: strs(f, "columns"),
                ref_table: s(f, "to"),
                ref_columns: strs(f, "ref_columns"),
                on_delete: f["on_delete"].as_str().map(str::to_string),
            })
            .collect(),
    }
}

fn decode_op(o: &Value) -> Op {
    let kind = o["op"].as_str().expect("op");
    let t = || s(o, "table");
    match kind {
        "create_table" | "adopt_table" => {
            let table = table_decl(&o["table"]);
            let indexes = o["indexes"]
                .as_array()
                .expect("indexes")
                .iter()
                .map(index)
                .collect();
            if kind == "create_table" {
                Op::CreateTable { table, indexes }
            } else {
                Op::AdoptTable { table, indexes }
            }
        }
        "add_column" => Op::AddColumn {
            table: t(),
            column: column(&o["column"]),
            fill: None,
        },
        "drop_column" => Op::DropColumn {
            table: t(),
            was: column(&o["was"]),
        },
        "alter_column" => Op::AlterColumn {
            table: t(),
            from: column(&o["from"]),
            to: column(&o["to"]),
        },
        "add_index" => Op::AddIndex {
            index: index(&o["index"]),
            concurrently: None,
        },
        "add_check" => Op::AddCheck {
            table: t(),
            check: check(&o["check"]),
        },
        other => panic!("unknown op `{other}`"),
    }
}

#[must_use]
pub fn decode(v: &Value) -> LoadedFile {
    LoadedFile {
        app: s(v, "app"),
        id: s(v, "id"),
        parents: strs(v, "parents"),
        depends_on: strs(v, "depends_on"),
        ops: v["operations"]
            .as_array()
            .expect("operations")
            .iter()
            .map(decode_op)
            .collect(),
    }
}

// --------------------------------------------------------------------- DAG

/// Validate (parents exist, no duplicate id, one leaf per app, no cycle) and
/// return the canonical order: topological, ties by `(app, id)`.
///
/// # Errors
/// The first broken rule.
pub fn order(files: &[LoadedFile]) -> Result<Vec<usize>, String> {
    let mut by_key: HashMap<(&str, &str), usize> = HashMap::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        if by_key.insert((&f.app, &f.id), i).is_some() {
            return Err(format!("duplicate id {}:{}", f.app, f.id));
        }
    }
    let mut indeg = vec![0usize; files.len()];
    let mut kids: Vec<Vec<usize>> = vec![vec![]; files.len()];
    for (i, f) in files.iter().enumerate() {
        let parents = f.parents.iter().map(|p| (f.app.as_str(), p.as_str()));
        let deps = f
            .depends_on
            .iter()
            .map(|d| d.split_once(':').expect("app:id"));
        for (app, id) in parents.chain(deps) {
            let p = *by_key
                .get(&(app, id))
                .ok_or_else(|| format!("{}:{} has no parent {app}:{id}", f.app, f.id))?;
            kids[p].push(i);
            indeg[i] += 1;
        }
    }
    let mut leaves: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        if !kids[i].iter().any(|&k| files[k].app == f.app) {
            *leaves.entry(&f.app).or_default() += 1;
        }
    }
    if let Some((app, n)) = leaves.iter().find(|(_, n)| **n != 1) {
        return Err(format!("{app}: {n} leaves"));
    }
    let mut heap: BinaryHeap<Reverse<(&str, &str, usize)>> = indeg
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| Reverse((files[i].app.as_str(), files[i].id.as_str(), i)))
        .collect();
    let mut out = Vec::with_capacity(files.len());
    while let Some(Reverse((_, _, i))) = heap.pop() {
        out.push(i);
        for &k in &kids[i] {
            indeg[k] -= 1;
            if indeg[k] == 0 {
                heap.push(Reverse((files[k].app.as_str(), files[k].id.as_str(), k)));
            }
        }
    }
    if out.len() != files.len() {
        return Err("cycle".into());
    }
    Ok(out)
}

// ------------------------------------------------------------ replay, plan

/// Replay `files[i]` for each `i` in `idx`, in order, onto `st`.
///
/// # Errors
/// The first op that does not replay.
pub fn replay_onto(st: &mut State, files: &[LoadedFile], idx: &[usize]) -> Result<(), String> {
    for &i in idx {
        for op in &files[i].ops {
            apply(st, op).map_err(|e| format!("{}:{} {e}", files[i].app, files[i].id))?;
        }
    }
    Ok(())
}

/// Render `idx` files in order from `st` (each via `render_file`). Returns
/// the final state and the unit count.
///
/// # Errors
/// The first op that fails to replay or render.
pub fn plan_onto(
    st: State,
    files: &[LoadedFile],
    idx: &[usize],
    d: &dyn Dialect,
) -> Result<(State, usize), String> {
    let (mut st, mut units) = (st, 0);
    for &i in idx {
        let (u, after) = render_file(&st, &files[i].ops, d)
            .map_err(|(n, e)| format!("{}:{} op {n}: {e}", files[i].app, files[i].id))?;
        units += u.len();
        st = after;
    }
    Ok((st, units))
}

/// What a per-op deep copy of the state costs (the pre-`Arc` behaviour).
#[must_use]
pub fn deep_clone(s: &State) -> State {
    State {
        tables: s
            .tables
            .iter()
            .map(|(k, v)| (k.clone(), Arc::new((**v).clone())))
            .collect(),
        externs: s.externs.clone(),
        extensions: s.extensions.clone(),
    }
}

/// Per-table hash memo keyed by `Arc` identity: a state hash re-hashes only
/// the tables an op replaced.
#[derive(Default)]
pub struct HashMemo(HashMap<usize, [u8; 32]>);

impl HashMemo {
    pub fn state_hash(&mut self, st: &State) -> [u8; 32] {
        let mut h = Sha256::new();
        for (name, t) in &st.tables {
            let key = Arc::as_ptr(t) as usize;
            let th = *self.0.entry(key).or_insert_with(|| table_hash(t));
            h.update(name.as_bytes());
            h.update(th);
        }
        h.finalize().into()
    }
}

#[must_use]
pub fn table_hash(t: &TableState) -> [u8; 32] {
    Sha256::digest(format!("{t:?}").as_bytes()).into()
}

/// Names of the tables a state holds, for assertions.
#[must_use]
pub fn table_names(s: &State) -> BTreeSet<&str> {
    s.tables.keys().map(String::as_str).collect()
}
