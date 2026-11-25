use super::*;
use serde_json::{json, Value};

/// Every row of the 2.6 table for `b`: `(ty, max_length, default)`.
pub(crate) fn defaults_table(b: Backend) -> Vec<(&'static str, Option<u32>, String)> {
    let mut rows: Vec<(&str, Option<u32>, String)> = vec![
        ("string", Some(10), "'en'".into()),
        ("string", Some(10), "'it''s'".into()),
        ("i32", None, "0".into()),
        ("decimal", None, "0.00".into()),
        ("bool", None, "TRUE".into()),
        ("bool", None, "FALSE".into()),
        ("date", None, "'2026-01-01'".into()),
    ];
    let own: [(&str, Option<u32>, String); 5] = match b {
        Backend::Postgres => [
            ("string", None, "'en'".into()),
            ("datetime", None, "now()".into()),
            ("uuid", None, "gen_random_uuid()".into()),
            ("json", None, "'{}'::jsonb".into()),
            ("json", None, "'[]'::jsonb".into()),
        ],
        Backend::Mysql => [
            ("string", None, "('en')".into()),
            ("datetime", None, "(UTC_TIMESTAMP(6))".into()),
            ("uuid", None, "(UUID())".into()),
            ("json", None, "('{}')".into()),
            ("json", None, "('[]')".into()),
        ],
        Backend::Sqlite => [
            ("string", None, "'en'".into()),
            (
                "datetime",
                None,
                format!("(strftime('{}','now'))", crate::sql::SQLITE_DATETIME_FORMAT),
            ),
            ("uuid", None, crate::sql::SQLITE_UUID_V4.to_owned()),
            ("json", None, "'{}'".into()),
            ("json", None, "'[]'".into()),
        ],
    };
    rows.extend(own);
    rows
}

/// A project whose one table has a column per `(ty, max_length, default)`.
pub(crate) fn project_with(dir: &Path, cols: &[(&str, Option<u32>, String)]) {
    let mut fields =
        vec![json!({"column": "id", "ty": "i64", "primary_key": true, "nullable": false})];
    for (i, (ty, max, default)) in cols.iter().enumerate() {
        let mut f = json!({"column": format!("c{i}"), "ty": ty, "nullable": true,
                           "default": default});
        if let Some(m) = max {
            f["max_length"] = json!(m);
        }
        fields.push(f);
    }
    let file = json!({"engine": 2, "format": 1, "id": "0001_initial", "app": "project",
        "parents": [], "target": "tenant", "operations": [{"op": "create_table", "v": 1,
        "table": {"name": "ck_defaults", "scope": "tenant", "fields": fields}}]});
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("0001_initial.json"), file.to_string()).unwrap();
}

fn errors(f: &[Finding]) -> Vec<&Finding> {
    f.iter().filter(|f| f.severity == Severity::Error).collect()
}

#[tokio::test]
async fn every_defaults_table_row_passes_offline_on_its_backend() {
    for b in [Backend::Postgres, Backend::Mysql, Backend::Sqlite] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        project_with(&dir, &defaults_table(b));
        let found = check(&dir, &[b], None, None).await.unwrap();
        assert!(found.is_empty(), "{b:?}: {found:#?}");
    }
}

#[tokio::test]
async fn a_string_default_sqlite_refuses_fails_naming_sqlite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    project_with(&dir, &[("string", Some(10), "'en'::text".into())]);
    let found = check(&dir, &[Backend::Postgres, Backend::Sqlite], None, None)
        .await
        .unwrap();
    let errs = errors(&found);
    assert!(!errs.is_empty(), "{found:#?}");
    assert!(errs.iter().all(|f| f.msg.contains("sqlite")), "{errs:#?}");
    // Postgres alone takes it.
    let found = check(&dir, &[Backend::Postgres], None, None).await.unwrap();
    assert!(errors(&found).is_empty(), "{found:#?}");
}

/// The in-memory SQLite run catches what the offline rules let through.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn the_in_memory_sqlite_probe_runs_the_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    project_with(&dir, &[("i32", None, "(no_such_fn())".into())]);
    let found = check(&dir, &[Backend::Sqlite], None, None).await.unwrap();
    let errs = errors(&found);
    assert_eq!(errs.len(), 1, "{found:#?}");
    assert!(errs[0].msg.contains("on sqlite"), "{errs:#?}");
}

#[tokio::test]
async fn the_policy_and_mysql_rules_name_their_backend() {
    let cases: [(Backend, Vec<Backend>, (&str, Option<u32>, String), &str); 3] = [
        (
            Backend::Sqlite,
            vec![Backend::Sqlite],
            ("datetime", None, "CURRENT_TIMESTAMP".into()),
            "policy",
        ),
        (
            Backend::Mysql,
            vec![Backend::Mysql],
            ("string", None, "'en'".into()),
            "parenthesised",
        ),
        (
            Backend::Mysql,
            vec![Backend::Postgres, Backend::Mysql],
            ("string", Some(10), "'a\\b'".into()),
            "backslash",
        ),
    ];
    for (b, all, col, want) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        project_with(&dir, std::slice::from_ref(&col));
        let found = check(&dir, &all, None, None).await.unwrap();
        let errs = errors(&found);
        assert_eq!(errs.len(), 1, "{b:?} {col:?}: {found:#?}");
        assert!(errs[0].msg.contains(want), "{errs:#?}");
    }
}

#[tokio::test]
async fn models_ahead_of_the_chain_is_a_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    project_with(&dir, &[]);
    let models: SchemaSnapshot = serde_json::from_value::<Value>(json!({"tables": [
        {"name": "ck_defaults", "model": "Ck", "fields": [
            {"name": "id", "column": "id", "ty": "i64", "primary_key": true, "nullable": false},
            {"name": "extra", "column": "extra", "ty": "i32", "nullable": true,
             "primary_key": false}]}]}))
    .and_then(serde_json::from_value)
    .unwrap();
    let found = check(&dir, &[Backend::Postgres], Some(&models), None)
        .await
        .unwrap();
    let errs = errors(&found);
    assert_eq!(errs.len(), 1, "{found:#?}");
    assert!(
        errs[0].msg.contains("add_column ck_defaults.extra"),
        "{errs:#?}"
    );
}

#[tokio::test]
async fn a_bare_reserved_name_in_a_check_fails_naming_the_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let file = json!({"engine": 2, "format": 1, "id": "0001_initial", "app": "project",
        "parents": [], "target": "tenant", "operations": [{"op": "create_table", "v": 1,
        "table": {"name": "ck_r", "scope": "tenant", "fields": [
            {"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
            {"column": "rank", "ty": "i32", "nullable": true},
            {"column": "d", "ty": "date", "nullable": true}]},
        "checks": [{"name": "ck_r_rank", "expr": "rank >= 0 AND d <= CURRENT_DATE"}]}]});
    std::fs::write(dir.join("0001_initial.json"), file.to_string()).unwrap();
    let found = check(&dir, &[Backend::Postgres, Backend::Mysql], None, None)
        .await
        .unwrap();
    let errs = errors(&found);
    assert_eq!(errs.len(), 1, "{found:#?}");
    assert!(
        errs[0].msg.contains("`rank` is reserved on mysql"),
        "{errs:#?}"
    );
    // PostgreSQL alone takes `rank` bare.
    let found = check(&dir, &[Backend::Postgres], None, None).await.unwrap();
    assert!(errors(&found).is_empty(), "{found:#?}");
}
