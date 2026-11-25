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
