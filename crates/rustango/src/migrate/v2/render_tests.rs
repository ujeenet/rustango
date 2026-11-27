//! Render goldens per op on PG, MySQL and SQLite (the S3 cases, in v2
//! file form), rendered with no pool, plus the render rules. The live
//! apply-and-probe half is in `render_live_tests`.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::format::{decode, File};
use super::render::{golden, render_file, Unit, UnitKind};
use super::replay::Ctx;
use super::state::{Backend, State, Target};
use super::{Code, LoadError};

pub(super) const CTX: Ctx<'static> = Ctx {
    target: Target::Tenant,
    registry: None,
};

pub(super) fn file(ops: &Value) -> File {
    decode(
        &json!({"engine": 2, "format": 1, "id": "0001_case", "app": "project",
                "target": "tenant", "operations": ops}),
        "case.json",
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

pub(super) fn render(
    state: &State,
    ops: &Value,
    b: Backend,
) -> Result<(Vec<Unit>, State), LoadError> {
    render_file(state, &file(ops), "case.json", &CTX, b)
}

/// A column: `name ty`, nullable, plus `extra` keys.
fn col(name: &str, ty: &str, extra: Value) -> Value {
    let mut c = json!({"column": name, "ty": ty, "nullable": true});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            c[k] = v;
        }
    }
    c
}

fn id() -> Value {
    col("id", "i64", json!({"primary_key": true, "nullable": false}))
}

fn create(name: &str, fields: Value, rest: Value) -> Value {
    let mut op = json!({"op": "create_table", "v": 1,
        "table": {"name": name, "scope": "tenant", "fields": fields}});
    if let Value::Object(m) = rest {
        for (k, v) in m {
            if k == "comment" {
                op["table"]["comment"] = v;
            } else {
                op[k] = v;
            }
        }
    }
    op
}

fn parent() -> Value {
    create(
        "s3_parent",
        json!([
            col(
                "id",
                "i64",
                json!({"primary_key": true, "auto": true, "nullable": false})
            ),
            col("name", "string", json!({"max_length": 50}))
        ]),
        json!({}),
    )
}

fn child_fk(on_delete: Option<&str>) -> Value {
    let mut fk = json!({"to": "s3_parent", "on": "id", "name": "s3_child_parent_fk"});
    if let Some(o) = on_delete {
        fk["on_delete"] = json!(o);
    }
    fk
}

fn child_idx() -> Value {
    json!({"name": "s3_child_parent_idx", "columns": ["parent_id"]})
}

fn child(fk: Option<Value>) -> Value {
    let mut pid = col("parent_id", "i64", json!({"nullable": false}));
    if let Some(f) = fk {
        pid["fk"] = f;
    }
    create(
        "s3_child",
        json!([id(), pid]),
        json!({"indexes": [child_idx()]}),
    )
}

fn s3_t(cols: Value, rest: Value) -> Value {
    let mut fields = vec![id()];
    fields.extend(cols.as_array().unwrap().iter().cloned());
    create("s3_t", Value::Array(fields), rest)
}

/// The "now" default as each backend's file writes it (2.6).
fn now(b: Backend) -> &'static str {
    match b {
        Backend::Postgres => "now()",
        Backend::Mysql => "(UTC_TIMESTAMP(6))",
        Backend::Sqlite => "(strftime('%Y-%m-%dT%H:%M:%f000+00:00','now'))",
    }
}

/// One column per 2.6 value, written for `b`.
pub(super) fn defaults_cols(b: Backend) -> Vec<Value> {
    let d = |name: &str, ty: &str, def: &str, extra: Value| {
        let mut c = col(name, ty, extra);
        c["default"] = json!(def);
        c
    };
    let mut v = vec![
        d("s", "string", "'en'", json!({"max_length": 20})),
        d("q", "string", "'it''s'", json!({"max_length": 20})),
        d("i", "i64", "0", json!({})),
        d("d", "decimal", "0.00", json!({})),
        d("b", "bool", "TRUE", json!({})),
        d("dt", "date", "'2026-01-01'", json!({})),
        d("ts", "datetime", now(b), json!({})),
    ];
    let sqlite_uuid = crate::sql::Dialect::translate_default_expr(
        &crate::sql::Sqlite,
        "gen_random_uuid()",
        "uuid",
        None,
    );
    v.extend(match b {
        Backend::Postgres => vec![
            d("u", "uuid", "gen_random_uuid()", json!({})),
            d("j", "json", "'{}'::jsonb", json!({})),
            d("ja", "json", "'[]'::jsonb", json!({})),
        ],
        Backend::Mysql => vec![
            d("lt", "string", "('en')", json!({})),
            d("u", "uuid", "(UUID())", json!({})),
            d("j", "json", "('{}')", json!({})),
            d("ja", "json", "('[]')", json!({})),
        ],
        Backend::Sqlite => vec![
            d("u", "uuid", &sqlite_uuid, json!({})),
            d("j", "json", "'{}'", json!({})),
            d("ja", "json", "'[]'", json!({})),
        ],
    });
    v
}

pub(super) struct Case {
    pub(super) name: &'static str,
    pub(super) setup: Value,
    /// Tables the setup created that the chain under test does not own.
    pub(super) forget: Vec<&'static str>,
    pub(super) seed: Vec<&'static str>,
    pub(super) ops: Value,
}

fn case(name: &'static str, setup: Value, seed: Vec<&'static str>, ops: Value) -> Case {
    Case {
        name,
        setup,
        forget: vec![],
        seed,
        ops,
    }
}

const SEED_PARENT: &str = "INSERT INTO s3_parent (name) VALUES ('a')";

#[allow(clippy::too_many_lines)]
pub(super) fn cases(b: Backend) -> Vec<Case> {
    let check_t = || {
        s3_t(
            json!([col("a", "i32", json!({}))]),
            json!({"checks": [{"name": "s3_t_a_pos", "expr": "a > 0"}]}),
        )
    };
    let created_child = create(
        "s3_child",
        json!([id(),
               {"column": "parent_id", "ty": "i64", "nullable": false, "fk": child_fk(Some("cascade"))},
               col("total", "decimal", json!({"nullable": false, "default": "0"})),
               col("note", "string", json!({"max_length": 20, "comment": "a note"}))]),
        json!({"indexes": [child_idx()],
               "checks": [{"name": "s3_child_total_ge0", "expr": "total >= 0"}],
               "comment": "children"}),
    );
    let retype_fk = json!({"to": "s3_t", "on": "id", "name": "s3_child_pid_fk"});
    vec![
        case(
            "create_table",
            json!([parent()]),
            vec![],
            json!([created_child]),
        ),
        case(
            "drop_table",
            json!([s3_t(
                json!([col("v", "string", json!({"max_length": 10}))]),
                json!({})
            )]),
            vec![],
            json!([{"op": "drop_table", "v": 1, "table": "s3_t", "was": {
                "name": "s3_t", "scope": "tenant", "pg_pkey": "s3_t_pkey",
                "columns": [{"column": "id", "ty": "i64", "nullable": false, "primary_key": true},
                            {"column": "v", "ty": "string", "nullable": true, "max_length": 10}]}}]),
        ),
        case(
            "rename_table",
            json!([parent()]),
            vec![SEED_PARENT],
            // The second table gets `s3_parent_pkey1`: the renamed one keeps its pkey name.
            json!([{"op": "rename_table", "v": 1, "from": "s3_parent", "to": "s3_renamed"},
                   parent()]),
        ),
        case(
            "alter_table_comment",
            json!([parent()]),
            vec![],
            json!([{"op": "alter_table_comment", "v": 1, "table": "s3_parent",
                    "from": null, "to": "people"}]),
        ),
        case(
            "add_column",
            json!([parent()]),
            vec![SEED_PARENT],
            json!([
                {"op": "add_column", "v": 1, "table": "s3_parent",
                 "column": col("nick", "string", json!({"max_length": 20, "comment": "short name"}))},
                {"op": "add_column", "v": 1, "table": "s3_parent",
                 "column": col("flag", "bool", json!({"nullable": false, "default": "TRUE"}))},
                {"op": "add_column", "v": 1, "table": "s3_parent",
                 "column": col("score", "i32", json!({"nullable": false})), "fill": "0"},
                {"op": "add_column", "v": 1, "table": "s3_parent",
                 "column": col("created", "datetime", json!({"default": now(b)}))}
            ]),
        ),
        case(
            "drop_column",
            json!([s3_t(
                json!([col(
                    "legacy",
                    "bool",
                    json!({"nullable": false, "default": "FALSE"})
                )]),
                json!({})
            )]),
            vec![],
            json!([{"op": "drop_column", "v": 1, "table": "s3_t", "column": "legacy",
                    "was": col("legacy", "bool", json!({"nullable": false, "default": "FALSE"}))}]),
        ),
        case(
            "rename_column",
            json!([s3_t(
                json!([col("name", "string", json!({"max_length": 30}))]),
                json!({"indexes": [{"name": "s3_t_name_idx", "columns": ["name"]}]})
            )]),
            vec![],
            json!([{"op": "rename_column", "v": 1, "table": "s3_t", "from": "name", "to": "full_name"}]),
        ),
        case(
            "alter_column_widen_not_null",
            json!([parent()]),
            vec![SEED_PARENT],
            json!([{"op": "alter_column", "v": 1, "table": "s3_parent", "column": "name",
                    "changes": {"max_length": {"from": 50, "to": 100},
                                "nullable": {"from": true, "to": false}}}]),
        ),
        case(
            "alter_column_type_noncastable_default",
            json!([s3_t(
                json!([col(
                    "code",
                    "string",
                    json!({"max_length": 10, "default": "'7'"})
                )]),
                json!({})
            )]),
            vec!["INSERT INTO s3_t (id, code) VALUES (1, '42')"],
            json!([{"op": "alter_column", "v": 1, "table": "s3_t", "column": "code",
                    "changes": {"ty": {"from": "string", "to": "i64"},
                                "max_length": {"from": 10, "to": null},
                                "default": {"from": "'7'", "to": "0"}}}]),
        ),
        case(
            "alter_column_default",
            json!([s3_t(
                json!([col(
                    "status",
                    "string",
                    json!({"max_length": 10, "nullable": false, "default": "'new'"})
                )]),
                json!({})
            )]),
            vec![],
            json!([{"op": "alter_column", "v": 1, "table": "s3_t", "column": "status",
                    "changes": {"default": {"from": "'new'", "to": "'open'"}}}]),
        ),
        case(
            "add_index",
            json!([s3_t(
                json!([
                    col("a", "i32", json!({})),
                    col("b", "string", json!({"max_length": 20}))
                ]),
                json!({})
            )]),
            vec![],
            json!([
                {"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_a_idx", "columns": ["a"]},
                {"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_b_uq", "columns": ["b"], "unique": true},
                {"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_a_pos", "columns": ["a"],
                 "where": "a > 0", "skipped_on": ["mysql"]},
                {"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_ab_idx", "columns": ["a", "b"],
                 "concurrently": false}
            ]),
        ),
        case(
            "drop_index",
            json!([s3_t(
                json!([col("a", "i32", json!({}))]),
                json!({"indexes": [{"name": "s3_t_a_idx", "columns": ["a"]}]})
            )]),
            vec![],
            json!([{"op": "drop_index", "v": 1, "table": "s3_t", "name": "s3_t_a_idx",
                    "was": {"name": "s3_t_a_idx", "columns": ["a"]}}]),
        ),
        case(
            "rename_index",
            json!([s3_t(
                json!([col("a", "i32", json!({}))]),
                json!({"indexes": [{"name": "s3_t_a_idx", "columns": ["a"]}]})
            )]),
            vec![],
            json!([{"op": "rename_index", "v": 1, "table": "s3_t", "from": "s3_t_a_idx", "to": "s3_t_a_idx2"}]),
        ),
        case(
            "add_check",
            json!([s3_t(json!([col("a", "i32", json!({}))]), json!({}))]),
            vec!["INSERT INTO s3_t (id, a) VALUES (1, 5)"],
            json!([{"op": "add_check", "v": 1, "table": "s3_t", "name": "s3_t_a_pos", "expr": "a > 0"}]),
        ),
        case(
            "drop_check",
            json!([check_t()]),
            vec![],
            json!([{"op": "drop_check", "v": 1, "table": "s3_t", "name": "s3_t_a_pos", "was": "a > 0"}]),
        ),
        case(
            "add_fk",
            json!([parent(), child(None)]),
            vec![
                SEED_PARENT,
                "INSERT INTO s3_child (id, parent_id) VALUES (1, 1)",
            ],
            json!([{"op": "add_fk", "v": 1, "table": "s3_child", "column": "parent_id",
                    "fk": child_fk(Some("cascade"))}]),
        ),
        case(
            "drop_fk",
            json!([parent(), child(Some(child_fk(None)))]),
            vec![],
            json!([{"op": "drop_fk", "v": 1, "table": "s3_child", "column": "parent_id",
                    "was": child_fk(None)}]),
        ),
        case(
            // An on_delete change is drop_fk + add_fk of the same name.
            "change_on_delete",
            json!([parent(), child(Some(child_fk(None)))]),
            vec![],
            json!([{"op": "drop_fk", "v": 1, "table": "s3_child", "column": "parent_id",
                    "was": child_fk(None)},
                   {"op": "add_fk", "v": 1, "table": "s3_child", "column": "parent_id",
                    "fk": child_fk(Some("restrict"))}]),
        ),
        case(
            // 3780: drop_fk -> retype both ends -> add_fk.
            "fk_retype",
            json!([
                create(
                    "s3_t",
                    json!([col(
                        "id",
                        "i32",
                        json!({"primary_key": true, "nullable": false})
                    )]),
                    json!({})
                ),
                create(
                    "s3_child",
                    json!([id(), {"column": "pid", "ty": "i32", "nullable": true, "fk": retype_fk}]),
                    json!({"indexes": [{"name": "s3_child_pid_idx", "columns": ["pid"]}]})
                )
            ]),
            vec![],
            json!([{"op": "drop_fk", "v": 1, "table": "s3_child", "column": "pid", "was": retype_fk},
                   {"op": "alter_column", "v": 1, "table": "s3_child", "column": "pid",
                    "changes": {"ty": {"from": "i32", "to": "i64"}}},
                   {"op": "alter_column", "v": 1, "table": "s3_t", "column": "id",
                    "changes": {"ty": {"from": "i32", "to": "i64"}}},
                   {"op": "add_fk", "v": 1, "table": "s3_child", "column": "pid", "fk": retype_fk}]),
        ),
        case(
            // 3959: drop_check -> rename_column -> add_check with the new text.
            "rename_column_under_check",
            json!([check_t()]),
            vec![],
            json!([{"op": "drop_check", "v": 1, "table": "s3_t", "name": "s3_t_a_pos", "was": "a > 0"},
                   {"op": "rename_column", "v": 1, "table": "s3_t", "from": "a", "to": "b"},
                   {"op": "add_check", "v": 1, "table": "s3_t", "name": "s3_t_b_pos", "expr": "b > 0"}]),
        ),
        case(
            "raw_sql",
            json!([s3_t(
                json!([col("name", "string", json!({"max_length": 50}))]),
                json!({})
            )]),
            vec![],
            json!([
                {"op": "raw_sql", "v": 1, "id": "name_raw", "tx": "autocommit", "probe": "state",
                 "up": {"any": "CREATE INDEX s3_t_name_raw ON {{t:s3_t}} ({{c:name}})"},
                 "down": {"any": "DROP INDEX s3_t_name_raw"},
                 "state": [{"op": "add_index", "v": 1, "table": "s3_t",
                     "name": "s3_t_name_raw", "columns": ["name"]}]},
                {"op": "raw_sql", "v": 1, "id": "name_hash", "tx": "autocommit", "probe": "state",
                 "up": {"postgres": "CREATE INDEX CONCURRENTLY s3_t_name_hash ON {{t:s3_t}} USING hash ({{c:name}})",
                        "mysql": null, "sqlite": null},
                 "down": {"postgres": "DROP INDEX CONCURRENTLY s3_t_name_hash"},
                 "state": [{"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_name_hash",
                            "columns": ["name"], "method": "hash", "skipped_on": ["mysql", "sqlite"]}]},
                {"op": "raw_sql", "v": 1, "id": "touch", "up": {"any": "UPDATE {{t:s3_t}} SET {{c:name}} = 'x' WHERE 1 = 0"},
                 "state": []}
            ]),
        ),
        case(
            "state_only",
            json!([s3_t(json!([col("a", "i32", json!({}))]), json!({})),
                   {"op": "raw_sql", "v": 1, "id": "by_hand", "tx": "autocommit",
                    "probe": {"idempotent": false, "sql": {"any": "SELECT 1"}},
                    "up": {"any": "CREATE INDEX s3_t_a_idx ON s3_t (a)"}, "state": []}]),
            vec![],
            json!([{"op": "state_only", "v": 1, "reason": "index built by hand", "ops": [
                {"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_a_idx", "columns": ["a"]}]}]),
        ),
        Case {
            name: "extern_and_adopt_table",
            setup: json!([
                create(
                    "s3_ext",
                    json!([id(), col("label", "string", json!({"max_length": 20}))]),
                    json!({})
                ),
                create(
                    "s3_adopt",
                    json!([id(), col("label", "string", json!({"max_length": 20}))]),
                    json!({})
                )
            ]),
            forget: vec!["s3_ext", "s3_adopt"],
            seed: vec![],
            ops: json!([
                {"op": "extern_table", "v": 1, "table": "s3_ext", "owner": "system",
                 "columns": [{"name": "id", "ty": "i64", "unique": true}]},
                {"op": "adopt_table", "v": 1,
                 "table": {"name": "s3_adopt", "scope": "tenant",
                           "fields": [id(), col("label", "string", json!({"max_length": 20}))]}},
                create("s3_child", json!([id(),
                    {"column": "ext_id", "ty": "i64", "nullable": false,
                     "fk": {"to": "s3_ext", "on": "id", "name": "s3_child_ext_fk"}}]),
                    json!({"indexes": [{"name": "s3_child_ext_idx", "columns": ["ext_id"]}]}))
            ]),
        },
        case(
            "create_extension",
            json!([]),
            vec![],
            json!([{"op": "create_extension", "v": 1, "name": "pg_trgm", "skipped_on": ["mysql", "sqlite"]}]),
        ),
        case(
            "defaults_as_written",
            json!([]),
            vec![],
            json!([create(
                "s3_defaults",
                {
                    let mut v = vec![id()];
                    v.extend(defaults_cols(b));
                    Value::Array(v)
                },
                json!({})
            )]),
        ),
    ]
}

/// The setup applied, then the forgotten tables removed from state.
pub(super) fn setup_state(case: &Case, b: Backend) -> (Vec<Unit>, State) {
    let (units, mut s0) = render(&State::default(), &case.setup, b)
        .unwrap_or_else(|e| panic!("{} setup on {}: {e}", case.name, b.as_str()));
    for t in &case.forget {
        s0.tables.remove(*t);
    }
    (units, s0)
}

pub(super) fn render_case(case: &Case, b: Backend) -> (State, Vec<Unit>, State) {
    let (_, s0) = setup_state(case, b);
    let (units, s1) = render(&s0, &case.ops, b)
        .unwrap_or_else(|e| panic!("{} on {}: {e}", case.name, b.as_str()));
    (s0, units, s1)
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/migrate_v2")
}

/// Every case on every backend matches its golden, rendered with no pool.
/// `MV2_BLESS=1` rewrites them.
#[test]
fn goldens_render_without_a_pool() {
    let bless = std::env::var_os("MV2_BLESS").is_some();
    let names: Vec<&str> = cases(Backend::Postgres).iter().map(|c| c.name).collect();
    let mut failed = Vec::new();
    for name in names {
        let mut text = String::new();
        for b in Backend::ALL {
            let all = cases(b);
            let case = all.iter().find(|c| c.name == name).unwrap();
            let (_, units, _) = render_case(case, b);
            text.push_str(&format!("-- {}\n{}", b.as_str(), golden(&units)));
        }
        let path = golden_dir().join(format!("{name}.sql"));
        if bless {
            std::fs::create_dir_all(golden_dir()).unwrap();
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {}; run with MV2_BLESS=1", path.display()));
        if want != text {
            failed.push(format!("{name}:\n--- want\n{want}--- got\n{text}"));
        }
    }
    assert!(failed.is_empty(), "goldens differ:\n{}", failed.join("\n"));
}

/// No rendered statement uses `IF [NOT] EXISTS` (resume relies on probes),
/// except `create_extension`; raw SQL is the user's own text.
#[test]
fn no_if_exists_in_rendered_sql() {
    for b in Backend::ALL {
        for case in cases(b) {
            if matches!(case.name, "create_extension" | "raw_sql") {
                continue;
            }
            let (_, units, _) = render_case(&case, b);
            for s in units.iter().flat_map(|u| &u.sql) {
                assert!(
                    !s.contains("IF EXISTS") && !s.contains("IF NOT EXISTS"),
                    "{}: {s}",
                    case.name
                );
            }
        }
    }
}

/// MySQL: one statement per unit, each autocommit with a probe, and one
/// statement per op except `add_column` with `fill` (three units, 1054).
#[test]
fn mysql_renders_one_statement_per_op() {
    let mut multi = Vec::new();
    for case in cases(Backend::Mysql) {
        let (_, units, _) = render_case(&case, Backend::Mysql);
        let n = case.ops.as_array().unwrap().len();
        for i in 0..n {
            let mine: Vec<&Unit> = units.iter().filter(|u| u.op == i).collect();
            for u in &mine {
                assert_eq!(u.sql.len(), 1, "{}#{i}", case.name);
                if u.kind == UnitKind::Autocommit {
                    assert!(u.probe.is_some(), "{}#{i} has no probe", case.name);
                }
            }
            if mine.len() > 1 {
                multi.push(format!("{}#{i}", case.name));
            }
        }
    }
    assert_eq!(multi, ["add_column#2"]);
}

/// MySQL `add_column` + `fill`: ADD nullable (probe: nullable), UPDATE in
/// a transaction, MODIFY NOT NULL (probe: NOT NULL).
#[test]
fn mysql_add_column_with_fill_is_three_units() {
    let all = cases(Backend::Mysql);
    let case = all.iter().find(|c| c.name == "add_column").unwrap();
    let (_, units, _) = render_case(case, Backend::Mysql);
    let fill: Vec<&Unit> = units.iter().filter(|u| u.op == 2).collect();
    let kinds: Vec<UnitKind> = fill.iter().map(|u| u.kind).collect();
    assert_eq!(
        kinds,
        [UnitKind::Autocommit, UnitKind::Tx, UnitKind::Autocommit]
    );
    assert!(
        fill[1].sql[0].starts_with("UPDATE `s3_parent` SET `score` = 0"),
        "{:?}",
        fill[1]
    );
    let nullable = |u: &Unit| match u.probe.as_deref() {
        Some([super::render::Expect::Column { column, .. }]) => column.nullable,
        other => panic!("{other:?}"),
    };
    assert!(nullable(fill[0]));
    assert!(!nullable(fill[2]));
}

fn expect_err(state: &State, ops: &Value, b: Backend, code: Code) -> LoadError {
    match render(state, ops, b) {
        Ok((u, _)) => panic!(
            "{}: expected {}, rendered {}",
            b.as_str(),
            code.as_str(),
            golden(&u)
        ),
        Err(e) => {
            assert_eq!(e.code, code, "{}: {e}", b.as_str());
            e
        }
    }
}

fn state_of(setup: &Value) -> State {
    render(&State::default(), setup, Backend::Postgres)
        .unwrap()
        .1
}

/// An autocommit unit with no probe for the state after it is a load error.
#[test]
fn an_autocommit_unit_without_a_probe_is_refused() {
    let s0 = state_of(&json!([s3_t(
        json!([col("a", "i32", json!({}))]),
        json!({})
    )]));
    // The probe SQL is written for PG only, so MySQL has none.
    let ops = json!([{"op": "raw_sql", "v": 1, "id": "ix", "tx": "autocommit",
        "probe": {"sql": {"postgres": "SELECT 1"}},
        "up": {"any": "CREATE INDEX s3_t_a_idx ON s3_t (a)"}, "state": []}]);
    render(&s0, &ops, Backend::Postgres).unwrap();
    let e = expect_err(&s0, &ops, Backend::Mysql, Code::Probe);
    assert!(e.msg.contains("mysql"), "{e}");
}

#[test]
fn raw_sql_rules() {
    let s0 = state_of(&json!([s3_t(
        json!([col("a", "i32", json!({}))]),
        json!({})
    )]));
    let raw = |sql: &str, extra: Value| {
        let mut op = json!({"op": "raw_sql", "v": 1, "id": "x", "up": {"any": sql}, "state": []});
        if let Value::Object(m) = extra {
            for (k, v) in m {
                op[k] = v;
            }
        }
        json!([op])
    };
    for b in Backend::ALL {
        expect_err(
            &s0,
            &raw("PRAGMA foreign_keys = OFF", json!({})),
            b,
            Code::Raw,
        );
        expect_err(
            &s0,
            &raw(
                "PRAGMA foreign_keys = OFF",
                json!({"tx": "autocommit", "probe": {"idempotent": true}}),
            ),
            b,
            Code::Raw,
        );
        expect_err(
            &s0,
            &raw("SET search_path TO other", json!({})),
            b,
            Code::Raw,
        );
        render(&s0, &raw("UPDATE s3_t SET a = 1", json!({})), b).unwrap();
        // Any SET outlives the unit (or commits it on MySQL).
        for set in ["SET x = 1", "SET autocommit = 1", "set names utf8mb4"] {
            expect_err(&s0, &raw(set, json!({})), b, Code::Raw);
        }
    }
    render(
        &s0,
        &raw("SET LOCAL lock_timeout = 100", json!({})),
        Backend::Postgres,
    )
    .unwrap();
    expect_err(
        &s0,
        &raw("FLUSH TABLES", json!({})),
        Backend::Mysql,
        Code::Raw,
    );
    // A backslash ends nothing in a PG or SQLite string: a second statement shows.
    let sneaky = raw("SELECT 'x\\'; SET search_path TO public; --'", json!({}));
    for b in [Backend::Postgres, Backend::Sqlite] {
        expect_err(&s0, &sneaky, b, Code::Raw);
    }
    // A dollar-quoted body is one string.
    render(&s0, &raw("SELECT $$a; b$$", json!({})), Backend::Postgres).unwrap();
    // MySQL commits DDL itself: a transactional raw DDL is refused there.
    let ddl = raw("CREATE INDEX s3_t_a_idx ON s3_t (a)", json!({}));
    expect_err(&s0, &ddl, Backend::Mysql, Code::Raw);
    render(&s0, &ddl, Backend::Postgres).unwrap();
}

/// PG's online NOT NULL check is `<t>_<c>_v2nn`, within 63 bytes.
#[test]
fn the_v2nn_name_fits_63_bytes() {
    let t = "t".repeat(40);
    let c = "c".repeat(20);
    let s0 = state_of(&json!([create(
        &t,
        json!([id(), col(&c, "i32", json!({}))]),
        json!({})
    )]));
    let ops = json!([{"op": "alter_column", "v": 1, "table": t, "column": c,
        "changes": {"nullable": {"from": true, "to": false}}}]);
    expect_err(&s0, &ops, Backend::Postgres, Code::Identifier);
    // MySQL and SQLite need no temporary check.
    render(&s0, &ops, Backend::Mysql).unwrap();
}

#[test]
fn dialect_refusals() {
    let s0 = state_of(&json!([parent(), child(Some(child_fk(None)))]));
    // A partial index on MySQL needs `skipped_on`.
    let partial = json!([{"op": "add_index", "v": 1, "table": "s3_child", "name": "s3_p",
        "columns": ["parent_id"], "where": "parent_id > 0"}]);
    expect_err(&s0, &partial, Backend::Mysql, Code::Dialect);
    render(&s0, &partial, Backend::Postgres).unwrap();
    let ext = json!([{"op": "create_extension", "v": 1, "name": "citext"}]);
    expect_err(&s0, &ext, Backend::Mysql, Code::Dialect);
    expect_err(&s0, &ext, Backend::Sqlite, Code::Dialect);
    // An FK whose only index is skipped on MySQL: MySQL would add a hidden one.
    let skipped_idx = json!([
        parent(),
        create(
            "s3_child",
            json!([id(),
        {"column": "parent_id", "ty": "i64", "nullable": false, "fk": child_fk(None)}]),
            json!({"indexes": [{"name": "s3_child_parent_idx", "columns": ["parent_id"],
                            "skipped_on": ["mysql"]}]})
        )
    ]);
    // It is a state rule, so every backend refuses it.
    for b in Backend::ALL {
        expect_err(&State::default(), &skipped_idx, b, Code::FkIndex);
    }
    // `using` is not enabled yet.
    let using = json!([{"op": "alter_column", "v": 1, "table": "s3_parent", "column": "name",
        "changes": {"ty": {"from": "string", "to": "text"}}, "using": {"any": "name"}}]);
    expect_err(&s0, &using, Backend::Postgres, Code::NotEnabled);
}

/// `concurrently` is three-state on PG: absent follows "did the table exist
/// before this file", `true` and `false` force it.
#[test]
fn concurrently_is_three_state() {
    let t = || s3_t(json!([col("a", "i32", json!({}))]), json!({}));
    let ix = |c: Value| {
        let mut op = json!({"op": "add_index", "v": 1, "table": "s3_t", "name": "s3_t_a_idx", "columns": ["a"]});
        if !c.is_null() {
            op["concurrently"] = c;
        }
        op
    };
    let s0 = state_of(&json!([t()]));
    let first = |units: Vec<Unit>| units[0].sql[0].contains("CONCURRENTLY");
    let r = |s: &State, ops: Value| render(s, &ops, Backend::Postgres).unwrap().0;
    assert!(first(r(&s0, json!([ix(Value::Null)]))), "existing table");
    assert!(!first(r(&s0, json!([ix(json!(false))]))));
    let fresh = State::default();
    let last = |units: Vec<Unit>| units.last().unwrap().sql[0].contains("CONCURRENTLY");
    assert!(
        !last(r(&fresh, json!([t(), ix(Value::Null)]))),
        "created in this file"
    );
    assert!(last(r(&fresh, json!([t(), ix(json!(true))]))));
}

/// An empty `probe: "state"` is no probe (review fix 6).
#[test]
fn an_empty_state_probe_is_no_probe() {
    let s0 = state_of(&json!([s3_t(
        json!([col("a", "i32", json!({}))]),
        json!({})
    )]));
    let ops = json!([{"op": "raw_sql", "v": 1, "id": "noop", "tx": "autocommit",
        "probe": "state", "up": {"any": "UPDATE s3_t SET a = 1"}, "state": []}]);
    for b in Backend::ALL {
        expect_err(&s0, &ops, b, Code::Probe);
    }
}

/// Comments do not hide a statement, and one op is one statement (review fix 9).
#[test]
fn raw_sql_guards_read_past_comments() {
    let s0 = state_of(&json!([s3_t(
        json!([col("a", "i32", json!({}))]),
        json!({})
    )]));
    let raw =
        |sql: &str| json!([{"op": "raw_sql", "v": 1, "id": "x", "up": {"any": sql}, "state": []}]);
    for b in Backend::ALL {
        expect_err(&s0, &raw("/* x */ PRAGMA foreign_keys = OFF"), b, Code::Raw);
        expect_err(&s0, &raw("-- note\nBEGIN"), b, Code::Raw);
        expect_err(
            &s0,
            &raw("UPDATE s3_t SET a = 1; UPDATE s3_t SET a = 2"),
            b,
            Code::Raw,
        );
        // A `;` inside a string or a trailing `;` is still one statement.
        render(&s0, &raw("UPDATE s3_t SET a = length('x;y');"), b).unwrap();
    }
    for sql in [
        "/* lead */ CREATE INDEX s3_t_a_idx ON s3_t (a)",
        "GRANT SELECT ON s3_t TO someone",
        "REVOKE SELECT ON s3_t FROM someone",
        "LOCK TABLES s3_t WRITE",
        "UNLOCK TABLES",
        "ANALYZE TABLE s3_t",
        "OPTIMIZE TABLE s3_t",
        "REPAIR TABLE s3_t",
    ] {
        expect_err(&s0, &raw(sql), Backend::Mysql, Code::Raw);
    }
}

/// A unique constraint renders as one: inline at create, `DROP CONSTRAINT`
/// on PG, a rebuild on SQLite (review fix 8).
#[test]
fn a_unique_constraint_renders_as_a_constraint() {
    let s0 = state_of(&json!([s3_t(
        json!([col("a", "i32", json!({}))]),
        json!({})
    )]));
    let ix = json!({"name": "s3_t_a_key", "columns": ["a"], "unique": true, "constraint": true});
    let mut add = ix.clone();
    add["op"] = json!("add_index");
    add["v"] = json!(1);
    add["table"] = json!("s3_t");
    let (pg, _) = render(&s0, &json!([add.clone()]), Backend::Postgres).unwrap();
    assert!(
        pg[0].sql[0].contains("ADD CONSTRAINT \"s3_t_a_key\" UNIQUE"),
        "{pg:?}"
    );
    let (lite, _) = render(&s0, &json!([add.clone()]), Backend::Sqlite).unwrap();
    assert_eq!(lite[0].kind, UnitKind::Rebuild, "{lite:?}");
    assert!(
        lite[0].sql[0].contains("CONSTRAINT \"s3_t_a_key\" UNIQUE"),
        "{lite:?}"
    );
    let s1 = state_of(&json!([
        s3_t(json!([col("a", "i32", json!({}))]), json!({}),),
        add
    ]));
    let drop = json!([{"op": "drop_index", "v": 1, "table": "s3_t", "name": "s3_t_a_key",
        "was": ix}]);
    let (pg, _) = render(&s1, &drop, Backend::Postgres).unwrap();
    assert!(
        pg[0].sql[0].contains("DROP CONSTRAINT \"s3_t_a_key\""),
        "{pg:?}"
    );
    let (lite, _) = render(&s1, &drop, Backend::Sqlite).unwrap();
    assert_eq!(lite[0].kind, UnitKind::Rebuild, "{lite:?}");
}

/// 6.1 rule 6: PG and SQLite run consecutive transactional schema ops in one
/// unit; a data op, an autocommit unit and MySQL DDL keep their own.
#[test]
fn pg_and_sqlite_merge_consecutive_schema_units() {
    let ops = json!([
        create("mg_t", json!([id(), col("a", "i32", json!({}))]), json!({})),
        {"op": "add_column", "v": 1, "table": "mg_t", "column": col("b", "i32", json!({}))},
        {"op": "data_sql", "v": 1, "id": "fill_v1", "tables": {"write": ["mg_t.b"]},
         "up": {"any": "UPDATE {{t:mg_t}} SET {{c:b}} = 1"},
         "down": {"noop": "created here"}},
        {"op": "add_column", "v": 1, "table": "mg_t", "column": col("c", "i32", json!({}))},
        {"op": "add_column", "v": 1, "table": "mg_t", "column": col("d", "i32", json!({}))}
    ]);
    let f = file(&ops);
    let kinds = |b| {
        let (units, _) =
            super::render::render_plan(&State::default(), &f, "case.json", &CTX, b).unwrap();
        units
            .iter()
            .map(|u| (u.kind, u.sql.len()))
            .collect::<Vec<_>>()
    };
    let merged = [(UnitKind::Tx, 2), (UnitKind::Data, 1), (UnitKind::Tx, 2)];
    assert_eq!(kinds(Backend::Postgres), merged);
    assert_eq!(kinds(Backend::Sqlite), merged);
    let my = kinds(Backend::Mysql);
    assert_eq!(my.len(), 5, "{my:?}");
    // The per-op goldens stay one statement per unit.
    let (per_op, _) = render(&State::default(), &ops, Backend::Postgres).unwrap();
    assert_eq!(per_op.len(), 5);
}

/// Under tenancy a registry file's extension goes to `extensions` (8.2);
/// another schema is refused.
#[test]
fn a_registry_extension_lives_in_extensions() {
    let reg = |ops: &Value| {
        let f = decode(
            &json!({"engine": 2, "format": 1, "id": "0001_case", "app": "project",
                    "target": "registry", "operations": ops}),
            "case.json",
        )
        .unwrap();
        let ctx = Ctx {
            target: Target::Registry,
            registry: None,
        };
        render_file(&State::default(), &f, "case.json", &ctx, Backend::Postgres)
    };
    let (units, _) = reg(&json!([{"op": "create_extension", "v": 1, "name": "citext"}])).unwrap();
    assert_eq!(
        units[0].sql,
        [
            "CREATE SCHEMA IF NOT EXISTS \"extensions\"",
            "CREATE EXTENSION IF NOT EXISTS \"citext\" SCHEMA \"extensions\""
        ]
    );
    let e = reg(&json!([{"op": "create_extension", "v": 1, "name": "citext", "schema": "public"}]))
        .unwrap_err();
    assert_eq!(e.code, Code::OtherSchema, "{e}");
}
