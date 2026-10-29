//! `makemigrations --drop-and-add` through the CLI and every `*_with` entry
//! point: a renamed-looking column is refused by default and written as
//! drop + add with the flag. Its own binary: it registers a model.

#![cfg(feature = "sqlite")]

use std::path::Path;

use rustango::core::{ModelEntry, ModelScope};
use rustango::migrate::{
    make_migrations_for_app_with, make_migrations_for_scope_with, make_migrations_from_with,
    make_migrations_scoped_with, make_migrations_with, manage, DropAndAdd, MigrateError,
    MigrationScope, SchemaSnapshot,
};
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "dacli_post", app = "dacli")]
#[allow(dead_code)]
pub struct DacliPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub tagline: Option<String>,
}

/// `snap` as it was before `tagline` replaced `subtitle`.
fn before_rename(mut snap: SchemaSnapshot) -> SchemaSnapshot {
    let post = snap
        .tables
        .iter_mut()
        .find(|t| t.name == "dacli_post")
        .expect("the test model");
    let col = post
        .fields
        .iter_mut()
        .find(|f| f.column == "tagline")
        .expect("tagline");
    col.name = "subtitle".into();
    col.column = "subtitle".into();
    snap
}

fn tenancy() -> bool {
    rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .any(|e| e.schema.scope == ModelScope::Registry && e.schema.managed)
}

/// Prior files that hold every model, `dacli_post` still with `subtitle`.
fn prior(dir: &Path, scoped: bool) {
    std::fs::create_dir_all(dir).unwrap();
    if scoped {
        for (model, file) in [
            (ModelScope::Registry, MigrationScope::Registry),
            (ModelScope::Tenant, MigrationScope::Tenant),
        ] {
            let mut snap = SchemaSnapshot::from_registry_for_scope(model);
            if model == ModelScope::Tenant {
                snap = before_rename(snap);
            }
            make_migrations_scoped_with(dir, &snap, model, file, None, DropAndAdd::Refuse)
                .expect("prior");
        }
    } else {
        let snap = before_rename(SchemaSnapshot::from_registry());
        make_migrations_from_with(dir, &snap, None, DropAndAdd::Refuse).expect("prior");
    }
}

async fn cli(dir: &Path, args: &[&str]) -> Result<String, MigrateError> {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let mut out = Vec::new();
    let mut argv = vec!["makemigrations".to_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    manage::run_with_writer(&pool, dir, argv, &mut out).await?;
    Ok(String::from_utf8(out).unwrap())
}

/// Refused without the flag, written as drop + add with it.
async fn refused_then_written(dir: &Path, args: &[&str]) {
    let err = cli(dir, args).await.expect_err("a likely rename");
    assert!(err.to_string().contains("--drop-and-add"), "{err}");
    let mut with = args.to_vec();
    with.push("--drop-and-add");
    let out = cli(dir, &with).await.expect("the drop + add, on purpose");
    assert!(
        out.contains("DropColumn") && out.contains("subtitle"),
        "{out}"
    );
}

/// The flagless path: one file without tenancy, the scope split with it.
#[tokio::test]
async fn cli_default_path_takes_drop_and_add() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    prior(&dir, tenancy());
    refused_then_written(&dir, &[]).await;
}

#[tokio::test]
async fn cli_scope_path_takes_drop_and_add() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let snap = before_rename(SchemaSnapshot::from_registry_for_scope(ModelScope::Tenant));
    make_migrations_scoped_with(
        &dir,
        &snap,
        ModelScope::Tenant,
        MigrationScope::Tenant,
        None,
        DropAndAdd::Refuse,
    )
    .expect("prior");
    refused_then_written(&dir, &["--scope", "tenant"]).await;
}

#[tokio::test]
async fn cli_app_path_takes_drop_and_add() {
    let root = tempfile::tempdir().unwrap();
    let app_dir = root.path().join("dacli").join("migrations");
    std::fs::create_dir_all(&app_dir).unwrap();
    let snap = before_rename(SchemaSnapshot::from_registry_for_app("dacli"));
    make_migrations_from_with(&app_dir, &snap, None, DropAndAdd::Refuse).expect("prior");
    refused_then_written(&root.path().join("migrations"), &["--app", "dacli"]).await;
}

/// The library entry points pass `drops` through.
#[test]
fn every_with_entry_point_passes_drops() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("single");
    prior(&dir, false);
    make_migrations_with(&dir, None, DropAndAdd::Refuse).expect_err("a likely rename");
    make_migrations_with(&dir, None, DropAndAdd::Allow)
        .unwrap()
        .expect("make_migrations_with");

    let dir = root.path().join("scoped");
    std::fs::create_dir_all(&dir).unwrap();
    let snap = before_rename(SchemaSnapshot::from_registry_for_scope(ModelScope::Tenant));
    make_migrations_scoped_with(
        &dir,
        &snap,
        ModelScope::Tenant,
        MigrationScope::Tenant,
        None,
        DropAndAdd::Refuse,
    )
    .expect("prior");
    make_migrations_for_scope_with(&dir, ModelScope::Tenant, None, DropAndAdd::Refuse)
        .expect_err("a likely rename");
    make_migrations_for_scope_with(&dir, ModelScope::Tenant, None, DropAndAdd::Allow)
        .unwrap()
        .expect("make_migrations_for_scope_with");

    let app_dir = root.path().join("dacli").join("migrations");
    std::fs::create_dir_all(&app_dir).unwrap();
    let snap = before_rename(SchemaSnapshot::from_registry_for_app("dacli"));
    make_migrations_from_with(&app_dir, &snap, None, DropAndAdd::Refuse).expect("prior");
    make_migrations_for_app_with(root.path(), "dacli", None, DropAndAdd::Refuse)
        .expect_err("a likely rename");
    make_migrations_for_app_with(root.path(), "dacli", None, DropAndAdd::Allow)
        .unwrap()
        .expect("make_migrations_for_app_with");
}
