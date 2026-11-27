//! Merge commutation (4.5): conflicting branches are refused at load with
//! both ops named; commuting ones load.

use serde_json::{json, Value};

use super::super::load::load_project;
use super::super::Code;

fn file(id: &str, parents: &[&str], ops: Value) -> Value {
    json!({"engine": 2, "format": 1, "id": id, "app": "project", "parents": parents,
           "target": "tenant", "operations": ops})
}

fn put(dir: &std::path::Path, id: &str, v: &Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
}

fn base(dir: &std::path::Path) {
    put(
        dir,
        "0001_t",
        &file(
            "0001_t",
            &[],
            json!([{"op": "create_table", "v": 1, "table": {"name": "t", "scope": "tenant",
                "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
                           {"column": "a", "ty": "i32", "nullable": true}]}}]),
        ),
    );
}

fn add_col(c: &str) -> Value {
    json!([{"op": "add_column", "v": 1, "table": "t",
        "column": {"column": c, "ty": "i32", "nullable": true}}])
}

fn branches(dir: &std::path::Path, left: Value, right: Value) {
    base(dir);
    put(dir, "0002_l", &file("0002_l", &["0001_t"], left));
    put(dir, "0002_r", &file("0002_r", &["0001_t"], right));
    put(
        dir,
        "0003_merge",
        &file("0003_merge", &["0002_l", "0002_r"], json!([])),
    );
}

#[test]
fn commuting_branches_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    branches(&dir, add_col("b"), add_col("c"));
    load_project(&dir).unwrap();
}

#[test]
fn conflicting_branches_are_refused_with_both_ops() {
    let cases = [
        // The same column, written twice.
        (add_col("b"), add_col("b")),
        // A column dropped on one side, indexed on the other.
        (
            json!([{"op": "drop_column", "v": 1, "table": "t", "column": "a",
                "was": {"column": "a", "ty": "i32", "nullable": true}}]),
            json!([{"op": "add_index", "v": 1, "table": "t", "name": "t_a_idx", "columns": ["a"]}]),
        ),
        // A check naming a column the other side renames.
        (
            json!([{"op": "rename_column", "v": 1, "table": "t", "from": "a", "to": "z"}]),
            json!([{"op": "add_check", "v": 1, "table": "t", "name": "t_a_ck", "expr": "a > 0"}]),
        ),
        // A data step writing the table the other side alters.
        (
            json!([{"op": "data_sql", "v": 1, "id": "d_v1", "tables": {"write": ["t"]},
                "up": {"any": "UPDATE {{t:t}} SET {{c:a}} = 1"}, "down": {"noop": "x"},
                "allow": [{"code": "MV2-E028", "reason": "x"}]}]),
            add_col("b"),
        ),
    ];
    for (left, right) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        branches(&dir, left.clone(), right.clone());
        let e = load_project(&dir).unwrap_err();
        assert_eq!(e.code, Code::Conflict, "{left} | {right}: {e}");
        assert!(
            e.msg.contains("project:0002_l") && e.msg.contains("project:0002_r"),
            "{e}"
        );
    }
}
