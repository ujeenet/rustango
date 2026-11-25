use super::*;
use serde_json::{json, Value};
use std::path::Path;

fn users() -> Value {
    json!({"op": "create_table", "v": 1, "table": {"name": "user", "scope": "tenant", "fields": [
        {"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
        {"column": "locale", "ty": "string", "max_length": 8, "nullable": true}]}})
}

fn other() -> Value {
    json!({"op": "create_table", "v": 1, "table": {"name": "other", "scope": "tenant", "fields": [
        {"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}})
}

fn data(extra: Value) -> Value {
    let mut op = json!({"op": "data_sql", "v": 1, "id": "fill_v1",
        "tables": {"read": ["user"], "write": ["user.locale"]},
        "up": {"any": "UPDATE {{t:user}} SET {{c:locale}} = {{?}} WHERE {{c:locale}} IS NULL"},
        "binds": [{"ty": "string", "value": "en"}],
        "down": {"noop": "the table is dropped by the reverse of op 0"}});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            op[k] = v;
        }
    }
    op
}

/// Load and replay a one-file project of `ops`.
fn load(ops: Value) -> Result<super::super::load::Project, LoadError> {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    write(&dir, ops);
    let p = super::super::load::load_project(&dir)?;
    super::super::load::replay(&p)?;
    Ok(p)
}

fn write(dir: &Path, ops: Value) {
    std::fs::create_dir_all(dir).unwrap();
    let f = json!({"engine": 2, "format": 1, "id": "0001_a", "app": "project",
        "parents": [], "target": "tenant", "operations": ops});
    std::fs::write(dir.join("0001_a.json"), f.to_string()).unwrap();
}

fn code(ops: Value) -> Code {
    load(ops).expect_err("refused").code
}

#[test]
fn placeholders_quote_per_dialect_and_number_binds() {
    let p = load(json!([users(), data(json!({}))])).unwrap();
    let want = [
        (
            Backend::Postgres,
            "UPDATE \"user\" SET \"locale\" = $1 WHERE \"locale\" IS NULL",
        ),
        (
            Backend::Mysql,
            "UPDATE `user` SET `locale` = ? WHERE `locale` IS NULL",
        ),
        (
            Backend::Sqlite,
            "UPDATE \"user\" SET \"locale\" = ? WHERE \"locale\" IS NULL",
        ),
    ];
    for (b, sql) in want {
        let units = super::super::render::render_project(&p, b).unwrap();
        let data: Vec<_> = units[0]
            .1
            .iter()
            .filter(|u| u.kind == super::super::render::UnitKind::Data)
            .collect();
        assert_eq!(data.len(), 1, "{b:?}");
        assert_eq!(data[0].sql, [sql], "{b:?}");
        assert_eq!(bind_values(&data[0].binds).unwrap(), [SqlValue::from("en")]);
    }
}

#[test]
fn names_outside_the_footprint_are_load_errors() {
    // `{{t:}}` of a table the footprint does not list.
    let up = json!({"up": {"any": "UPDATE {{t:other}} SET {{c:locale}} = 1"}, "binds": []});
    assert_eq!(code(json!([users(), other(), data(up)])), Code::Footprint);
    // A read of a table the footprint does not list.
    let read = json!({"up": {"any": "UPDATE {{t:user}} SET {{c:locale}} = (SELECT MAX(id) FROM {{t:other}})"},
        "binds": []});
    assert_eq!(code(json!([users(), other(), data(read)])), Code::Footprint);
    // A write through a table the footprint only reads.
    let ro = json!({"tables": {"read": ["user", "other"], "write": ["user.locale"]},
        "up": {"any": "DELETE FROM {{t:other}}"}, "binds": []});
    assert_eq!(code(json!([users(), other(), data(ro)])), Code::Footprint);
    // A table named bare, outside the footprint.
    let bare = json!({"up": {"any": "UPDATE {{t:user}} SET {{c:locale}} = 'x' WHERE id IN (SELECT id FROM other)"},
        "binds": []});
    assert_eq!(code(json!([users(), other(), data(bare)])), Code::Footprint);
    // A footprint table not in state.
    let gone = json!({"tables": {"read": ["nope"], "write": ["user.locale"]}});
    assert_eq!(code(json!([users(), data(gone)])), Code::Footprint);
}

#[test]
fn bad_placeholders_binds_and_batches_are_load_errors() {
    let unknown = json!({"up": {"any": "UPDATE {{t:user}} SET {{x:locale}} = 1"}, "binds": []});
    assert_eq!(code(json!([users(), data(unknown)])), Code::File);
    let count = json!({"binds": []});
    assert_eq!(code(json!([users(), data(count)])), Code::File);
    let ty = json!({"binds": [{"ty": "nope", "value": 1}]});
    assert_eq!(code(json!([users(), data(ty)])), Code::File);
    // A batched op without `{{batch_range}}`, and the marker without `batch`.
    let batch = json!({"batch": {"table": "user", "key": "id", "size": 10}});
    assert_eq!(code(json!([users(), data(batch)])), Code::File);
    let range = json!({"up": {"any": "UPDATE {{t:user}} SET {{c:locale}} = 'x' WHERE {{batch_range}}"},
        "binds": []});
    assert_eq!(code(json!([users(), data(range)])), Code::File);
    let auto = json!({"tx": "autocommit"});
    assert_eq!(code(json!([users(), data(auto)])), Code::File);
}

#[test]
fn a_noop_down_is_verified() {
    // The table comes from an earlier file: the reverse leaves the data.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    write(&dir, json!([users()]));
    let f = json!({"engine": 2, "format": 1, "id": "0002_b", "app": "project",
        "parents": ["0001_a"], "target": "tenant", "operations": [data(json!({}))]});
    std::fs::write(dir.join("0002_b.json"), f.to_string()).unwrap();
    let e = super::super::load::load_project(&dir).unwrap_err();
    assert_eq!(e.code, Code::Noop, "{e}");
    // An `allow` with a reason lets it pass.
    let mut op = data(json!({}));
    op["allow"] = json!([{"code": "MV2-E028", "reason": "values are recomputed at runtime"}]);
    let f = json!({"engine": 2, "format": 1, "id": "0002_b", "app": "project",
        "parents": ["0001_a"], "target": "tenant", "operations": [op]});
    std::fs::write(dir.join("0002_b.json"), f.to_string()).unwrap();
    super::super::load::load_project(&dir).unwrap();
    // Created earlier in the same file: verified.
    load(json!([users(), data(json!({}))])).unwrap();
}

#[test]
fn misses_name_every_unresolved_step() {
    let p = load(json!([
        users(),
        data(json!({"up": {"postgres": "UPDATE {{t:user}} SET {{c:locale}} = {{?}}"}})),
        data(json!({"id": "two", "down": {"irreversible": "one way"}}))
    ]))
    .unwrap();
    let f = &p.files[0].file;
    assert_eq!(misses(f, Backend::Postgres, false).len(), 0);
    assert_eq!(misses(f, Backend::Sqlite, false).len(), 1);
    let back = misses(f, Backend::Sqlite, true);
    assert_eq!(back.len(), 1, "{back:?}");
    assert!(back[0].contains("irreversible"), "{back:?}");
    let e = refuse_misses(&misses(f, Backend::Mysql, false), "plan").unwrap_err();
    assert_eq!(e.code, Code::Unresolved);
}

#[test]
fn a_batch_walks_one_unique_ordered_column() {
    let up = json!({"any": "UPDATE {{t:user}} SET {{c:locale}} = 'x' WHERE {{batch_range}}"});
    let ok = json!({"up": up.clone(), "binds": [],
        "batch": {"table": "user", "key": "id", "size": 10}});
    let p = load(json!([users(), data(ok)])).unwrap();
    let units = super::super::render::render_project(&p, Backend::Postgres).unwrap();
    let u = units[0].1.iter().find(|u| u.batch.is_some()).unwrap();
    assert!(u.sql[0].contains("{{batch_range}}"), "{:?}", u.sql);
    let bad = |batch: Value| json!({"up": up.clone(), "binds": [], "batch": batch});
    // A composite key, a nullable or non-unique column, a table outside the footprint.
    for b in [
        json!({"table": "user", "key": ["id", "locale"], "size": 10}),
        json!({"table": "user", "key": "locale", "size": 10}),
        json!({"table": "user", "key": "id", "size": 0}),
    ] {
        assert_eq!(
            code(json!([users(), data(bad(b.clone()))])),
            Code::File,
            "{b}"
        );
    }
    let outside = json!({"table": "other", "key": "id", "size": 1});
    assert_eq!(
        code(json!([users(), other(), data(bad(outside))])),
        Code::Footprint
    );
}

#[test]
fn batch_markers_expand_in_text_order() {
    let d: &dyn crate::sql::Dialect = &crate::sql::Postgres;
    let (sql, params) = expand(
        "UPDATE t SET a = {{?}} WHERE {{batch_range}} AND b = {{?}}",
        d,
        vec![SqlValue::from("x"), SqlValue::from("y")],
        "id",
        Some(&SqlValue::I64(5)),
        Some(&SqlValue::I64(9)),
    );
    assert_eq!(
        sql,
        "UPDATE t SET a = $1 WHERE (\"id\" > $2 AND \"id\" <= $3) AND b = $4"
    );
    assert_eq!(
        params,
        [
            SqlValue::from("x"),
            SqlValue::I64(5),
            SqlValue::I64(9),
            SqlValue::from("y")
        ]
    );
    let (sql, _) = expand("{{batch_range}}", d, Vec::new(), "id", None, None);
    assert_eq!(sql, "(1 = 1)");
}
