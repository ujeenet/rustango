//! The S2 budgets on a 2,000-file project: replay < 100 ms, and load plus
//! replay with state hashes < 2 s. Timed in release builds only; a debug
//! build still runs the fixture for correctness.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::load::{load_project, replay, replay_with};
use super::render::render_project;
use super::state::{Backend, HashMemo, Target};

const APPS: usize = 4;
const FILES_PER_APP: usize = 500;
const TABLES_PER_APP: usize = 40;

/// The budgets from section 4.4 of the design.
const REPLAY_BUDGET: Duration = Duration::from_millis(100);
const PLAN_BUDGET: Duration = Duration::from_secs(2);

/// Deterministic LCG, so the fixture is the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

fn col(name: &str, len: u32) -> Value {
    json!({"column": name, "ty": "string", "max_length": len, "nullable": true})
}

fn create(name: &str, parent: Option<&str>) -> Value {
    let mut fields = vec![
        json!({"column": "id", "ty": "i64", "primary_key": true, "auto": true, "nullable": false}),
    ];
    for c in 0..4 {
        fields.push(col(&format!("f{c}"), 60));
    }
    let mut indexes = vec![json!({"name": format!("{name}_f1_idx"), "columns": ["f1"]})];
    if let Some(p) = parent {
        fields.push(json!({"column": "parent_id", "ty": "i64", "nullable": true,
            "fk": {"to": p, "on": "id", "on_delete": "cascade", "name": format!("{name}_parent_fk")}}));
        indexes.push(json!({"name": format!("{name}_parent_idx"), "columns": ["parent_id"]}));
    }
    json!({"op": "create_table", "v": 1,
        "table": {"name": name, "scope": "tenant", "fields": fields},
        "indexes": indexes,
        "checks": [{"name": format!("{name}_f0_ck"), "expr": "length(f0) > 0"}]})
}

fn id(i: usize) -> String {
    format!("{:04}_step", i + 1)
}

/// Write the project: `APPS` app folders of `FILES_PER_APP` files, about
/// five ops each; apps after the first point FKs at the first app's tables.
fn write_fixture(root: &Path) {
    let mut rng = Rng(7);
    for a in 0..APPS {
        let app = format!("a{a}");
        let dir = if a == 0 {
            root.join("migrations")
        } else {
            root.join(&app).join("migrations")
        };
        std::fs::create_dir_all(&dir).unwrap();
        let mut tables: Vec<String> = Vec::new();
        let mut plain: Vec<(String, String, u32)> = Vec::new();
        for i in 0..FILES_PER_APP {
            let mut ops = Vec::new();
            if tables.len() < TABLES_PER_APP {
                for _ in 0..5 {
                    let name = format!("{app}_t{}", tables.len());
                    let parent = match (a, tables.is_empty()) {
                        (0, true) => None,
                        (0, false) => Some(tables[rng.next(tables.len())].clone()),
                        _ => Some(format!("a0_t{}", rng.next(5))),
                    };
                    ops.push(create(&name, parent.as_deref()));
                    tables.push(name);
                }
            } else {
                let t = tables[rng.next(tables.len())].clone();
                let t2 = tables[rng.next(tables.len())].clone();
                let (x, p) = (format!("x{a}_{i}"), format!("p{a}_{i}"));
                ops.push(json!({"op": "add_column", "v": 1, "table": t, "column": col(&x, 40)}));
                ops.push(json!({"op": "add_index", "v": 1, "table": t,
                    "name": format!("ix_{a}_{i}"), "columns": [x]}));
                ops.push(json!({"op": "add_check", "v": 1, "table": t,
                    "name": format!("ck_{a}_{i}"), "expr": format!("length({x}) > 0")}));
                ops.push(json!({"op": "add_column", "v": 1, "table": t2, "column": col(&p, 40)}));
                plain.push((t2, p, 40));
                if plain.len() > 3 {
                    let (pt, pc, w) = plain.remove(0);
                    if i % 2 == 0 {
                        ops.push(
                            json!({"op": "alter_column", "v": 1, "table": pt, "column": pc,
                            "changes": {"max_length": {"from": w, "to": 80}}}),
                        );
                        plain.push((pt, pc, 80));
                    } else {
                        ops.push(json!({"op": "drop_column", "v": 1, "table": pt,
                            "column": pc, "was": col(&pc, w)}));
                    }
                }
            }
            let parents: Vec<String> = if i == 0 { vec![] } else { vec![id(i - 1)] };
            let depends: Vec<String> = if a > 0 && i == 0 {
                vec![format!("a0:{}", id(0))]
            } else {
                vec![]
            };
            let file = json!({"engine": 2, "format": 1, "id": id(i), "app": app,
                "parents": parents, "depends_on": depends, "target": "tenant",
                "created_at": "2026-11-24T00:00:00Z", "operations": ops});
            std::fs::write(dir.join(format!("{}.json", id(i))), file.to_string()).unwrap();
        }
    }
}

#[test]
fn s2_budgets_on_2000_files() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture(tmp.path());
    let dir = tmp.path().join("migrations");

    let started = Instant::now();
    let project = load_project(&dir).unwrap();
    let replayed = replay(&project).unwrap();
    let plan = started.elapsed();
    assert_eq!(project.files.len(), APPS * FILES_PER_APP);
    let ops: usize = project.files.iter().map(|f| f.file.operations.len()).sum();
    assert!(ops >= 4 * project.files.len(), "{ops} ops");

    // Replay alone (the 100 ms budget), then with a state hash per file.
    let started = Instant::now();
    replay_with(&project, |_, _, _| {}).unwrap();
    let replay_time = started.elapsed();
    let started = Instant::now();
    let again = replay(&project).unwrap();
    let hashed = started.elapsed();

    // Deterministic: the same files give the same state.
    let mut memo = HashMemo::default();
    let tenant = |r: &super::load::Replayed| r.states[&Target::Tenant].clone();
    assert_eq!(
        tenant(&replayed).hash(&mut memo),
        tenant(&again).hash(&mut memo)
    );
    assert_eq!(tenant(&replayed).tables.len(), APPS * TABLES_PER_APP);

    // A fresh plan plus render per backend: load from disk, render every file.
    let mut renders = Vec::new();
    for b in Backend::ALL {
        let started = Instant::now();
        let project = load_project(&dir).unwrap();
        let units = render_project(&project, b).unwrap();
        renders.push((b, started.elapsed()));
        // The fixture's repeated widenings to 80 are no-op alters: they render nothing.
        let n: usize = units.iter().map(|(_, u)| u.len()).sum();
        assert!(n > 4 * project.files.len(), "{}: {n} units", b.as_str());
    }

    eprintln!(
        "S2: {ops} ops; load + replay + hashes {plan:?}; replay {replay_time:?}; \
         replay + hashes {hashed:?}; load + render {renders:?}"
    );
    if cfg!(debug_assertions) {
        return;
    }
    assert!(replay_time < REPLAY_BUDGET, "replay took {replay_time:?}");
    assert!(plan < PLAN_BUDGET, "load + replay + hashes took {plan:?}");
    for (b, took) in renders {
        assert!(
            took < PLAN_BUDGET,
            "{}: load + render took {took:?}",
            b.as_str()
        );
    }
}
