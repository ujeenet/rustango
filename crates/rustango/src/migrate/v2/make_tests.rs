use super::*;
use serde_json::json;

fn field(name: &str, ty: &str, extra: Value) -> Value {
    let mut f =
        json!({"name": name, "column": name, "ty": ty, "nullable": true, "primary_key": false});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            f[k] = v;
        }
    }
    f
}

fn id() -> Value {
    field(
        "id",
        "i64",
        json!({"primary_key": true, "auto": true, "nullable": false}),
    )
}

/// `mk_author` (unique email, a default, min/max) and `mk_book` (FK, no index).
fn models(rank: Value) -> SchemaSnapshot {
    serde_json::from_value(json!({"tables": [
        {"name": "mk_author", "model": "MkAuthor", "fields": [id(),
            field("name", "string", json!({"max_length": 50, "nullable": false, "default": "'x'"})),
            field("email", "string", json!({"max_length": 100, "unique": true})),
            field("rank", "i32", rank)]},
        {"name": "mk_book", "model": "MkBook", "fields": [id(),
            field("author_id", "i64", json!({"nullable": false,
                "fk": {"kind": "fk", "to": "mk_author", "on": "id", "on_delete": "CASCADE"}}))]},
    ]}))
    .unwrap()
}

fn opts() -> MakeOptions {
    MakeOptions {
        backends: vec![Backend::Postgres, Backend::Mysql, Backend::Sqlite],
        ..MakeOptions::default()
    }
}

fn ops_of(path: &Path) -> Vec<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["op"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn generating_twice_gives_identical_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    let m = models(json!({"min": 0, "max": 9}));
    let made = make(&dir, &dir, &m, &opts()).unwrap().unwrap();
    assert!(made.path.ends_with("0001_initial.json"), "{:?}", made.path);
    let first = std::fs::read(&made.path).unwrap();
    // The chain now matches the models.
    assert!(make(&dir, &dir, &m, &opts()).unwrap().is_none());
    std::fs::remove_file(&made.path).unwrap();
    let again = make(&dir, &dir, &m, &opts()).unwrap().unwrap();
    assert_eq!(std::fs::read(&again.path).unwrap(), first);
    // The uncovered FK got its index inside create_table, so it replays.
    let text = String::from_utf8(first).unwrap();
    assert!(text.contains("\"mk_book_author_id_idx\""), "{text}");
}

#[test]
fn min_and_max_lower_to_add_check() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    make(&dir, &dir, &models(json!({})), &opts())
        .unwrap()
        .unwrap();
    let made = make(&dir, &dir, &models(json!({"min": 0, "max": 9})), &opts())
        .unwrap()
        .unwrap();
    assert_eq!(ops_of(&made.path), ["add_check"]);
    let text = std::fs::read_to_string(&made.path).unwrap();
    let file: Value = serde_json::from_str(&text).unwrap();
    let op = &file["operations"][0];
    assert_eq!(
        (&op["column"], &op["min"], &op["max"]),
        (&json!("rank"), &json!(0), &json!(9)),
        "{text}"
    );
    assert!(op.get("expr").is_none(), "{text}");
    // Dropping the bound drops the check.
    let made = make(&dir, &dir, &models(json!({"min": 0})), &opts())
        .unwrap()
        .unwrap();
    assert_eq!(ops_of(&made.path), ["drop_check", "add_check"]);
}

#[test]
fn a_not_null_column_on_an_existing_table_needs_a_fill() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    make(&dir, &dir, &models(json!({})), &opts())
        .unwrap()
        .unwrap();
    let mut m = models(json!({}));
    let extra: FieldSnapshot =
        serde_json::from_value(field("pages", "i32", json!({"nullable": false}))).unwrap();
    m.tables[1].fields.push(extra);
    let e = make(&dir, &dir, &m, &opts()).unwrap_err();
    assert_eq!(e.code, Code::Fill, "{e}");
    let mut o = opts();
    o.fills.insert("mk_book.pages".into(), "0".into());
    let made = make(&dir, &dir, &m, &o).unwrap().unwrap();
    assert_eq!(ops_of(&made.path), ["add_column"]);
}

#[test]
fn a_default_with_two_spellings_is_refused_naming_the_backends() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    let mut m = models(json!({}));
    let at: FieldSnapshot =
        serde_json::from_value(field("at", "time", json!({"default": "now()"}))).unwrap();
    m.tables[0].fields.push(at);
    let e = make(&dir, &dir, &m, &opts()).unwrap_err();
    assert_eq!(e.code, Code::Dialect, "{e}");
    assert!(e.msg.contains("postgres") && e.msg.contains("mysql"), "{e}");
    // One backend: its own spelling.
    let one = MakeOptions {
        backends: vec![Backend::Sqlite],
        ..MakeOptions::default()
    };
    let made = make(&dir, &dir, &m, &one).unwrap().unwrap();
    let text = std::fs::read_to_string(&made.path).unwrap();
    assert!(text.contains("strftime"), "{text}");
}

#[test]
fn drops_retypes_and_fks_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    make(&dir, &dir, &models(json!({})), &opts())
        .unwrap()
        .unwrap();
    let mut m = models(json!({}));
    // Drop `email` (and its unique index), retype `mk_author.id` (the FK
    // into it is dropped and re-added), drop the FK's table comment no-op.
    m.tables[0].fields.retain(|f| f.column != "email");
    m.tables[0].fields[0].ty = "i32".into();
    m.tables[1].fields[1].ty = "i32".into();
    let made = make(&dir, &dir, &m, &opts()).unwrap().unwrap();
    assert_eq!(
        ops_of(&made.path),
        [
            "drop_fk",
            "drop_index",
            "drop_column",
            "alter_column",
            "alter_column",
            "add_fk"
        ]
    );
    // Dropping a table.
    m.tables.remove(1);
    let made = make(&dir, &dir, &m, &opts()).unwrap().unwrap();
    assert_eq!(ops_of(&made.path), ["drop_table"]);
    assert!(make(&dir, &dir, &m, &opts()).unwrap().is_none());
}

#[test]
fn a_v1_project_is_refused_with_the_adopt_pointer() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    let m = models(json!({}));
    crate::migrate::make::make_migrations_from(&dir, &m, None).unwrap();
    let e = make(&dir, &dir, &m, &opts()).unwrap_err();
    assert_eq!(e.code, Code::Engine, "{e}");
    assert!(e.msg.contains("manage dbmigrate adopt"), "{e}");
}

/// After `adopt` the chain matches the models it came from, on each backend.
#[test]
fn after_adopt_the_models_match() {
    for b in [Backend::Postgres, Backend::Mysql, Backend::Sqlite] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        let m = models(json!({"min": 0, "max": 9}));
        crate::migrate::make::make_migrations_from(&dir, &m, None).unwrap();
        super::super::adopt::adopt(&dir, b).unwrap();
        let one = MakeOptions {
            backends: vec![b],
            ..MakeOptions::default()
        };
        let made = make(&dir, &dir, &m, &one).unwrap();
        assert!(made.is_none(), "{b:?}: {:?}", made.map(|m| m.lines));
    }
}

#[test]
fn a_drop_and_an_add_of_one_type_needs_rename_or_no_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    make(&dir, &dir, &models(json!({"min": 0, "max": 9})), &opts())
        .unwrap()
        .unwrap();
    // `rank` becomes `level`: same type, with its range check.
    let mut m = models(json!({"min": 0, "max": 9}));
    for f in &mut m.tables[0].fields {
        if f.column == "rank" {
            f.column = "level".into();
            f.name = "level".into();
        }
    }
    let e = make(&dir, &dir, &m, &opts()).unwrap_err();
    assert_eq!(e.code, Code::Rename, "{e}");
    assert!(e.msg.contains("--rename mk_author.rank:level"), "{e}");
    let mut o = opts();
    o.renames = vec![("mk_author".into(), "nope".into(), "level".into())];
    assert_eq!(make(&dir, &dir, &m, &o).unwrap_err().code, Code::Rename);
    let dry = MakeOptions {
        no_rename: true,
        dry_run: true,
        ..opts()
    };
    let made = make(&dir, &dir, &m, &dry).unwrap().unwrap();
    assert!(
        made.lines
            .iter()
            .any(|l| l == "- drop_column mk_author.rank")
            && made
                .lines
                .iter()
                .any(|l| l == "+ add_column mk_author.level"),
        "{:?}",
        made.lines
    );
    o.renames = vec![("mk_author".into(), "rank".into(), "level".into())];
    let made = make(&dir, &dir, &m, &o).unwrap().unwrap();
    assert_eq!(
        ops_of(&made.path),
        ["drop_check", "rename_column", "add_check"]
    );
    assert!(make(&dir, &dir, &m, &opts()).unwrap().is_none());
}

#[test]
fn make_data_rust_writes_the_op_the_stub_and_the_mod_line() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("src/migrations");
    make(&dir, &dir, &models(json!({})), &opts())
        .unwrap()
        .unwrap();
    let made = make_data_rust(
        &dir,
        &dir,
        "fill_rank",
        &["mk_author".to_owned()],
        &["mk_author.rank".to_owned()],
    )
    .unwrap();
    assert!(
        made.path.ends_with("0002_fill_rank.json"),
        "{:?}",
        made.path
    );
    let op = &serde_json::from_str::<Value>(&std::fs::read_to_string(&made.path).unwrap()).unwrap()
        ["operations"][0];
    assert_eq!(op["op"], "data_rust");
    assert_eq!(op["id"], "fill_rank_v1");
    assert_eq!(op["tables"]["write"], json!(["mk_author.rank"]));
    let stub = std::fs::read_to_string(tmp.path().join("src/data_steps/fill_rank.rs")).unwrap();
    assert!(stub.contains("#[rustango::data_step(app = \"project\", id = \"fill_rank_v1\")]"));
    assert!(stub.contains("impl FillRank"), "{stub}");
    assert!(stub.contains("testing::at"), "{stub}");
    let mods = std::fs::read_to_string(tmp.path().join("src/data_steps/mod.rs")).unwrap();
    assert_eq!(mods, "pub mod fill_rank;\n");
    super::super::load::load_project(&dir).unwrap();
    // A footprint table not in state is refused, and nothing is written.
    let e = make_data_rust(&dir, &dir, "nope", &["ghost".to_owned()], &[]).unwrap_err();
    assert_eq!(e.code, Code::Footprint, "{e}");
    assert!(!dir.join("0003_nope.json").exists());
}

/// Under tenancy a tenant model's FK to a registry model is refused at
/// make, as replay refuses it (decision 4); a single database allows it.
#[cfg(feature = "tenancy")]
#[test]
fn a_tenant_fk_to_a_registry_model_is_refused_under_tenancy() {
    use crate::core::Model as _;
    let orgs = crate::tenancy::Org::SCHEMA.table;
    let m: SchemaSnapshot = serde_json::from_value(json!({"tables": [
        {"name": "inv", "model": "Inv", "fields": [id(),
            field("org_id", "i64", json!({"nullable": false,
                "fk": {"kind": "fk", "to": orgs, "on": "id", "on_delete": "CASCADE"}}))]},
    ]}))
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    let tenancy = MakeOptions {
        tenancy: true,
        dry_run: true,
        ..opts()
    };
    let e = make(&dir, &dir, &m, &tenancy).unwrap_err();
    assert_eq!(e.code, Code::CrossTarget, "{e}");
    assert!(e.msg.contains(&format!("registry table `{orgs}`")), "{e}");
}

/// Expand/contract (6.8): an add and a drop become two files, the drop in
/// a `phase: contract` file on top; `--no-split` keeps one contract file.
#[test]
fn contract_ops_split_into_their_own_file() {
    for split in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        make(&dir, &dir, &models(json!({})), &opts())
            .unwrap()
            .unwrap();
        let mut m = models(json!({}));
        m.tables[0].fields.retain(|f| f.column != "email");
        let bio: crate::migrate::snapshot::FieldSnapshot =
            serde_json::from_value(field("bio", "i32", json!({}))).unwrap();
        m.tables[0].fields.push(bio);
        let o = MakeOptions {
            no_split: !split,
            ..opts()
        };
        let made = make(&dir, &dir, &m, &o).unwrap().unwrap();
        let read = |p: &Path| -> Value {
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        if split {
            assert_eq!(files.len(), 3, "{files:?}");
            assert_eq!(ops_of(&made.path), ["add_column"]);
            assert!(read(&made.path).get("phase").is_none());
            let contract = read(&files[2]);
            assert_eq!(contract["phase"], "contract");
            assert_eq!(ops_of(&files[2]), ["drop_index", "drop_column"]);
        } else {
            assert_eq!(files.len(), 2, "{files:?}");
            assert_eq!(read(&made.path)["phase"], "contract");
        }
        super::super::load::load_project(&dir).unwrap();
        assert!(make(&dir, &dir, &m, &opts()).unwrap().is_none());
    }
}

/// A model default every backend spells its own way becomes a typed
/// `portable_default` instead of a refusal (design item 21).
#[test]
fn portable_model_defaults_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    let m: SchemaSnapshot = serde_json::from_value(json!({"tables": [
        {"name": "mk_pd", "model": "MkPd", "fields": [id(),
            field("at", "datetime", json!({"nullable": false, "default": "now()"})),
            field("on", "bool", json!({"nullable": false, "default": "true"})),
            field("prefs", "json", json!({"nullable": false, "default": "'{}'"}))]}]}))
    .unwrap();
    let made = make(&dir, &dir, &m, &opts()).unwrap().unwrap();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&made.path).unwrap()).unwrap();
    let fields = v["operations"][0]["table"]["fields"].as_array().unwrap();
    let pd = |c: &str| {
        fields
            .iter()
            .find(|f| f["column"] == c)
            .map(|f| (f["portable_default"].clone(), f.get("default").cloned()))
            .unwrap()
    };
    assert_eq!(pd("at"), (json!("now"), None));
    assert_eq!(pd("on"), (json!("true"), None));
    assert_eq!(pd("prefs"), (json!("empty_object"), None));
    assert!(make(&dir, &dir, &m, &opts()).unwrap().is_none());
}
