use super::*;
use serde_json::json;

async fn sqlite(tmp: &Path) -> Pool {
    let url = format!("sqlite:{}?mode=rwc", tmp.join("cli.db").display());
    Pool::connect(&url).await.unwrap()
}

async fn cli(pool: &Pool, dir: &Path, args: &[&str]) -> Result<String, MigrateError> {
    let mut out = Vec::new();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    crate::migrate::manage::run_with_writer(pool, dir, args, &mut out).await?;
    Ok(String::from_utf8(out).unwrap())
}

fn v2_project(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let create = json!({"op": "create_table", "v": 1, "table": {"name": "cl_t", "scope": "tenant",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}});
    let add = json!({"op": "add_column", "v": 1, "table": "cl_t",
        "column": {"column": "n", "ty": "i32", "nullable": true}});
    for (id, parents, op) in [
        ("0001_t", json!([]), create),
        ("0002_n", json!(["0001_t"]), add),
    ] {
        let f = json!({"engine": 2, "format": 1, "id": id, "app": "project",
            "parents": parents, "target": "tenant", "operations": [op]});
        std::fs::write(dir.join(format!("{id}.json")), f.to_string()).unwrap();
    }
}

#[tokio::test]
async fn every_dbmigrate_verb_refuses_a_v1_project() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("0001_initial.json"),
        r#"{"name": "0001_initial", "created_at": "", "snapshot": {"tables": []}, "forward": []}"#,
    )
    .unwrap();
    let pool = sqlite(tmp.path()).await;
    for verb in [
        &["dbmigrate", "make"][..],
        &["dbmigrate", "apply"],
        &["dbmigrate", "down"],
        &["dbmigrate", "status"],
        &["dbmigrate", "check", "--offline"],
        &["dbmigrate", "sql", "0001_initial"],
    ] {
        let e = cli(&pool, &dir, verb).await.unwrap_err().to_string();
        assert!(
            e.contains("`manage migrate`") && e.contains("`manage dbmigrate adopt`"),
            "{verb:?}: {e}"
        );
    }
}

#[tokio::test]
async fn the_verbs_run_a_v2_project() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    v2_project(&dir);
    let pool = sqlite(tmp.path()).await;
    let sql = cli(
        &pool,
        &dir,
        &["dbmigrate", "sql", "0002_n", "--dialect", "all"],
    )
    .await
    .unwrap();
    assert!(
        sql.contains("on postgres") && sql.contains("on mysql"),
        "{sql}"
    );
    assert!(sql.contains("ADD COLUMN"), "{sql}");
    let out = cli(&pool, &dir, &["dbmigrate", "apply", "--plan"])
        .await
        .unwrap();
    assert!(out.contains("CREATE TABLE"), "{out}");
    let out = cli(&pool, &dir, &["dbmigrate", "apply"]).await.unwrap();
    assert!(out.contains("Applied 0002_n"), "{out}");
    let out = cli(&pool, &dir, &["dbmigrate", "status"]).await.unwrap();
    assert_eq!(out, "[X] project:0001_t\n[X] project:0002_n\n");
    let out = cli(&pool, &dir, &["dbmigrate", "down", "--to", "0001_t"])
        .await
        .unwrap();
    assert_eq!(out, "Rolled back 0002_n\n");
    let out = cli(&pool, &dir, &["dbmigrate", "status"]).await.unwrap();
    assert_eq!(out, "[X] project:0001_t\n[ ] project:0002_n\n");
    let e = cli(&pool, &dir, &["dbmigrate", "nope"]).await.unwrap_err();
    assert!(e.to_string().contains("unknown verb"), "{e}");
    // The v1 group refuses this project and points here.
    let e = cli(&pool, &dir, &["migrate"]).await.unwrap_err();
    assert!(e.to_string().contains("manage dbmigrate"), "{e}");
}

/// A planned stop shows in status as stopped, resuming on the next run.
#[tokio::test]
async fn status_names_a_planned_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    v2_project(&dir);
    let pool = sqlite(tmp.path()).await;
    let stopping = crate::migrate::v2::run::Options {
        stop: Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        ))),
        ..crate::migrate::v2::run::Options::default()
    };
    let rep = crate::migrate::v2::run::migrate(&pool, &dir, &stopping)
        .await
        .unwrap();
    assert!(rep.stopped.is_some(), "{rep:?}");
    let out = cli(&pool, &dir, &["dbmigrate", "status"]).await.unwrap();
    assert_eq!(
        out,
        "[X] project:0001_t\n[!] project:0002_n (stopped, resumes on the next run)\n"
    );
    let out = cli(&pool, &dir, &["dbmigrate", "apply"]).await.unwrap();
    assert!(out.contains("Applied 0002_n"), "{out}");
}

/// `apply <earlier id>` rolls back only with --allow-rollback; a pending
/// target applies up to it (decision 6).
#[tokio::test]
async fn apply_to_an_earlier_target_needs_allow_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    v2_project(&dir);
    let pool = sqlite(tmp.path()).await;
    let out = cli(&pool, &dir, &["dbmigrate", "apply", "0001_t"])
        .await
        .unwrap();
    assert_eq!(out, "Applied 0001_t\n");
    cli(&pool, &dir, &["dbmigrate", "apply"]).await.unwrap();
    let e = cli(&pool, &dir, &["dbmigrate", "apply", "0001_t"])
        .await
        .unwrap_err();
    assert!(e.to_string().contains("--allow-rollback"), "{e}");
    let out = cli(&pool, &dir, &["dbmigrate", "status"]).await.unwrap();
    assert_eq!(out, "[X] project:0001_t\n[X] project:0002_n\n");
    let out = cli(
        &pool,
        &dir,
        &["dbmigrate", "apply", "0001_t", "--allow-rollback"],
    )
    .await
    .unwrap();
    assert_eq!(out, "Rolled back 0002_n\n");
}

/// `check --drift` compares the catalog with the state at the applied set:
/// clean after apply, a hand-added column is a difference.
#[tokio::test]
async fn check_drift_reports_a_hand_added_column() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    v2_project(&dir);
    let pool = sqlite(tmp.path()).await;
    cli(&pool, &dir, &["dbmigrate", "apply"]).await.unwrap();
    let out = cli(&pool, &dir, &["dbmigrate", "check", "--drift"])
        .await
        .unwrap();
    assert!(out.contains("No drift."), "{out}");
    crate::sql::raw_execute_pool(
        &pool,
        "ALTER TABLE cl_t ADD COLUMN stray INTEGER",
        Vec::new(),
    )
    .await
    .unwrap();
    let e = cli(&pool, &dir, &["dbmigrate", "check", "--drift"])
        .await
        .unwrap_err();
    assert!(e.to_string().contains("1 difference"), "{e}");
}

/// Two leaves: `make --merge` writes the merge node over them; `make
/// --rebase` moves one leaf on top of the other instead.
#[tokio::test]
async fn merge_and_rebase_join_two_leaves() {
    let leaf = |id: &str, c: &str| {
        json!({"engine": 2, "format": 1, "id": id, "app": "project", "parents": ["0002_n"],
            "target": "tenant", "operations": [{"op": "add_column", "v": 1, "table": "cl_t",
            "column": {"column": c, "ty": "i32", "nullable": true}}]})
    };
    for verb in ["--merge", "--rebase"] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        v2_project(&dir);
        for (id, c) in [("0003_a", "a"), ("0003_b", "b")] {
            std::fs::write(dir.join(format!("{id}.json")), leaf(id, c).to_string()).unwrap();
        }
        let pool = sqlite(tmp.path()).await;
        assert!(
            cli(&pool, &dir, &["dbmigrate", "apply"]).await.is_err(),
            "two leaves"
        );
        let args: &[&str] = if verb == "--merge" {
            &["dbmigrate", "make", "--merge"]
        } else {
            &["dbmigrate", "make", "--rebase", "0003_b"]
        };
        cli(&pool, &dir, args).await.unwrap();
        let expect = if verb == "--merge" {
            "0004_merge.json"
        } else {
            "0004_b.json"
        };
        assert!(dir.join(expect).exists(), "{verb}");
        cli(&pool, &dir, &["dbmigrate", "apply"]).await.unwrap();
    }
}

/// `MV2-W012` (6.8): a pending contract file whose expand file is pending
/// too; once the expand file ran, no warning. A batched data op next to a
/// contract op in one file is an error.
#[tokio::test]
async fn a_contract_file_shipped_with_its_expand_file_warns() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    v2_project(&dir);
    let pool = sqlite(tmp.path()).await;
    cli(&pool, &dir, &["dbmigrate", "apply"]).await.unwrap();
    let write = |id: &str, v: serde_json::Value| {
        std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
    };
    write(
        "0003_m",
        json!({"engine": 2, "format": 1, "id": "0003_m", "app": "project", "parents": ["0002_n"],
            "target": "tenant", "operations": [{"op": "add_column", "v": 1, "table": "cl_t",
            "column": {"column": "m", "ty": "i32", "nullable": true}}]}),
    );
    write(
        "0004_drop_n",
        json!({"engine": 2, "format": 1, "id": "0004_drop_n", "app": "project",
            "parents": ["0003_m"], "target": "tenant", "phase": "contract",
            "operations": [{"op": "drop_column", "v": 1, "table": "cl_t", "column": "n",
            "was": {"column": "n", "ty": "i32", "nullable": true}}]}),
    );
    let w012 = |found: &[crate::migrate::v2::check::Finding]| {
        found.iter().any(|f| f.msg.contains("MV2-W012"))
    };
    let found = crate::migrate::v2::check::check(&dir, &[Backend::Sqlite], None, Some(&pool))
        .await
        .unwrap();
    assert!(w012(&found), "{found:?}");
    let up_to = crate::migrate::v2::run::Options {
        up_to: Some("0003_m".into()),
        ..crate::migrate::v2::run::Options::default()
    };
    crate::migrate::v2::run::migrate(&pool, &dir, &up_to)
        .await
        .unwrap();
    let found = crate::migrate::v2::check::check(&dir, &[Backend::Sqlite], None, Some(&pool))
        .await
        .unwrap();
    assert!(!w012(&found), "{found:?}");
    // A batched data op with a contract op in one file.
    write(
        "0005_mix",
        json!({"engine": 2, "format": 1, "id": "0005_mix", "app": "project",
            "parents": ["0004_drop_n"], "target": "tenant", "operations": [
            {"op": "data_sql", "v": 1, "id": "b_v1", "tables": {"read": ["cl_t"], "write": ["cl_t.m"]},
             "up": {"any": "UPDATE {{t:cl_t}} SET {{c:m}} = 1 WHERE {{batch_range}}"},
             "batch": {"table": "cl_t", "key": "id", "size": 10},
             "down": {"noop": "x"}, "allow": [{"code": "MV2-E028", "reason": "x"}]},
            {"op": "drop_column", "v": 1, "table": "cl_t", "column": "m",
             "was": {"column": "m", "ty": "i32", "nullable": true}}]}),
    );
    let found = crate::migrate::v2::check::check(&dir, &[Backend::Sqlite], None, Some(&pool))
        .await
        .unwrap();
    assert!(
        found
            .iter()
            .any(|f| f.msg.contains("batched data op and a contract op")),
        "{found:?}"
    );
}
