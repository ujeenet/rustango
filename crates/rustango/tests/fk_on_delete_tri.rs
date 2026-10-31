//! FK `ON DELETE SET NULL` / `SET DEFAULT`, rendered through the snapshot
//! path migrations take and then run by each server, not string-matched.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{DeleteQuery, Filter, Model as _, ModelSchema, Op, WhereExpr};
use rustango::migrate::{detect_changes, render_changes_split_with_dialect, SchemaSnapshot};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::testkit::matrix::drop_table;
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_tri_author")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_tri_set_null")]
pub struct PostSetNull {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "fkod_tri_author", on = "id", on_delete = "set_null")]
    pub author_id: Option<i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_tri_set_default")]
pub struct PostSetDefault {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(
        fk = "fkod_tri_author",
        on = "id",
        on_delete = "set_default",
        default = "0"
    )]
    pub author_id: i64,
}

/// The snapshot render of `models` on `pool`'s dialect.
fn render(pool: &Pool, models: &[&'static ModelSchema]) -> Result<Vec<String>, String> {
    let current = SchemaSnapshot::from_models(models);
    let changes = detect_changes(&SchemaSnapshot::default(), &current);
    let batch = render_changes_split_with_dialect(&changes, &current, pool.dialect())?;
    let mut out = batch.immediate;
    out.extend(batch.deferred_fks);
    Ok(out)
}

fn set_default_runs(pool: &Pool) -> bool {
    pool.dialect().supports_on_delete_set_default()
}

/// Apply the snapshot DDL; MySQL gets no `SET DEFAULT` table, as its
/// writer refuses one InnoDB would not enforce.
async fn setup(pool: &Pool) {
    for table in [
        "fkod_tri_set_default",
        "fkod_tri_set_null",
        "fkod_tri_author",
    ] {
        drop_table(pool, table).await;
    }
    let mut models = vec![Author::SCHEMA, PostSetNull::SCHEMA];
    if set_default_runs(pool) {
        models.push(PostSetDefault::SCHEMA);
    }
    for stmt in render(pool, &models).expect("render") {
        rustango::sql::raw_execute_pool(pool, &stmt, Vec::new())
            .await
            .unwrap_or_else(|e| panic!("apply `{stmt}`: {e}"));
    }
}

/// Deleting the parent nulls or resets each child's key on the server.
async fn parent_delete_runs_the_declared_action(pool: &Pool) {
    for id in [0, 1] {
        Author { id }.insert_pool(pool).await.expect("author");
    }
    PostSetNull {
        id: 10,
        author_id: Some(1),
    }
    .insert_pool(pool)
    .await
    .expect("set null child");
    if set_default_runs(pool) {
        PostSetDefault {
            id: 20,
            author_id: 1,
        }
        .insert_pool(pool)
        .await
        .expect("set default child");
    }

    let parent = WhereExpr::Predicate(Filter::new("id", Op::Eq, 1_i64));
    rustango::sql::delete_pool(pool, &DeleteQuery::new(Author::SCHEMA, parent))
        .await
        .expect("the parent delete is allowed");

    let nulled = PostSetNull::objects().fetch(pool).await.expect("fetch");
    assert_eq!(nulled.len(), 1, "SET NULL keeps the child row");
    assert_eq!(nulled[0].author_id, None, "SET NULL did not run");
    if set_default_runs(pool) {
        let reset = PostSetDefault::objects().fetch(pool).await.expect("fetch");
        assert_eq!(reset.len(), 1, "SET DEFAULT keeps the child row");
        assert_eq!(reset[0].author_id, 0, "SET DEFAULT did not run");
    } else {
        let err = render(pool, &[Author::SCHEMA, PostSetDefault::SCHEMA])
            .expect_err("a backend that ignores SET DEFAULT refuses to render it");
        assert!(err.contains("set_default"), "{err}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [parent_delete_runs_the_declared_action],
}
