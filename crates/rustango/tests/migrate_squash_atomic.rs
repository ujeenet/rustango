//! `migrate --squash` keeps the pending files when the regenerated diff
//! fails. Its own binary: the composite FK below fails every fresh diff.

#![cfg(feature = "sqlite")]

use rustango::core::{ModelEntry, ModelScope};
use rustango::migrate::{
    file, make_migrations_from_with, make_migrations_scoped_with, manage, DropAndAdd,
    MigrationScope, SchemaSnapshot,
};
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sqat_parent", app = "sqat")]
#[allow(dead_code)]
pub struct SqatParent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub a: i64,
    pub b: i64,
}

/// `(a, b)` is no unique key of the parent: a fresh diff refuses it.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "sqat_child",
    app = "sqat",
    fk_composite(name = "sqat_pair", to = "sqat_parent", on = ("a", "b"), from = ("a", "b"))
)]
#[allow(dead_code)]
pub struct SqatChild {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub a: i64,
    pub b: i64,
}

/// Every file in `dir`, name to text.
fn contents(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&p).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn a_failed_squash_keeps_the_pending_files() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    // Files written without the FK, which the head snapshot then gains:
    // the models match the files, but a diff from scratch refuses it.
    let without_fk = |mut s: SchemaSnapshot| {
        for t in &mut s.tables {
            t.composite_fks.clear();
        }
        s
    };
    let scoped = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .any(|e| e.schema.scope == ModelScope::Registry && e.schema.managed);
    if scoped {
        for (model, file) in [
            (ModelScope::Registry, MigrationScope::Registry),
            (ModelScope::Tenant, MigrationScope::Tenant),
        ] {
            let current = without_fk(SchemaSnapshot::from_registry_for_scope(model));
            make_migrations_scoped_with(&dir, &current, model, file, None, DropAndAdd::Allow)
                .expect("scoped files");
        }
    } else {
        let current = without_fk(SchemaSnapshot::from_registry());
        make_migrations_from_with(&dir, &current, None, DropAndAdd::Allow).expect("file");
    }
    let registry = SchemaSnapshot::from_registry();
    let fk = registry
        .table("sqat_child")
        .expect("child")
        .composite_fks
        .clone();
    assert!(!fk.is_empty(), "the model declares the composite FK");
    for mut mig in file::list_dir(&dir).unwrap() {
        if let Some(t) = mig
            .snapshot
            .tables
            .iter_mut()
            .find(|t| t.name == "sqat_child")
        {
            t.composite_fks.clone_from(&fk);
            file::write(&dir.join(format!("{}.json", mig.name)), &mig).unwrap();
        }
    }
    manage::make_empty(&dir, "note").expect("--empty");
    let before = contents(&dir);
    assert!(before.len() >= 2, "squash needs two pending files");

    let args = vec!["migrate".to_owned(), "--squash".to_owned()];
    let err = manage::run_with_writer(&pool, &dir, args, &mut Vec::new())
        .await
        .expect_err("a fresh diff refuses the composite FK");
    assert!(err.to_string().contains("composite"), "{err}");
    assert_eq!(
        contents(&dir),
        before,
        "the pending files are back as they were"
    );
}
