//! `migrate --squash` refuses a bad relation target before it deletes the
//! pending files it would regenerate. Its own binary: the bad model below
//! fails every `makemigrations` the process runs.

#![cfg(feature = "sqlite")]

use rustango::core::{ModelEntry, ModelScope};
use rustango::migrate::{
    file, make_migrations_from_with, make_migrations_scoped_with, manage, DropAndAdd,
    MigrationScope, SchemaSnapshot,
};
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sqrc_author", app = "sqrc")]
#[allow(dead_code)]
pub struct SqrcAuthor {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
}

/// `fk` names the model, not its table: the relation check refuses it.
#[derive(Model, Debug, Clone)]
#[rustango(table = "sqrc_post", app = "sqrc")]
#[allow(dead_code)]
pub struct SqrcPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "SqrcAuthor")]
    pub author_id: i64,
}

#[tokio::test]
async fn squash_checks_relation_targets_before_deleting() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    // Pending files that already hold the models, written past the check
    // `makemigrations` would refuse them with.
    let scoped = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .any(|e| e.schema.scope == ModelScope::Registry && e.schema.managed);
    if scoped {
        for (model, file) in [
            (ModelScope::Registry, MigrationScope::Registry),
            (ModelScope::Tenant, MigrationScope::Tenant),
        ] {
            let current = SchemaSnapshot::from_registry_for_scope(model);
            make_migrations_scoped_with(&dir, &current, model, file, None, DropAndAdd::Allow)
                .expect("scoped files");
        }
    } else {
        make_migrations_from_with(
            &dir,
            &SchemaSnapshot::from_registry(),
            None,
            DropAndAdd::Allow,
        )
        .expect("file");
    }
    manage::make_empty(&dir, "note").expect("--empty");
    let before = file::list_dir(&dir).unwrap().len();
    assert!(before >= 2, "squash needs two pending files");

    let args = vec!["migrate".to_owned(), "--squash".to_owned()];
    let err = manage::run_with_writer(&pool, &dir, args, &mut Vec::new())
        .await
        .expect_err("a bad relation target");
    assert!(err.to_string().contains("relation targets"), "{err}");
    assert_eq!(
        file::list_dir(&dir).unwrap().len(),
        before,
        "nothing deleted"
    );
}
