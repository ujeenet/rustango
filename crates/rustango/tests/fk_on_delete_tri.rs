//! FK `ON DELETE SET NULL` / `SET DEFAULT` / `RESTRICT`, rendered through the
//! snapshot path migrations take and then run by each server, not string-matched.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{DeleteQuery, Filter, Model as _, ModelSchema, Op, WhereExpr};
use rustango::migrate::{detect_changes, render_changes_split_with_dialect, SchemaSnapshot};
use rustango::sql::{CounterPool as _, ExecError, FetcherPool as _, Pool};
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_tri_restrict")]
pub struct PostRestrict {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "fkod_tri_author", on = "id", on_delete = "restrict")]
    pub author_id: i64,
}

/// Two hops from the author, so a cascade must follow FKs transitively.
#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_tri_reply")]
pub struct Reply {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "fkod_tri_restrict", on = "id", on_delete = "restrict")]
    pub post_id: i64,
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
        "fkod_tri_reply",
        "fkod_tri_restrict",
        "fkod_tri_set_default",
        "fkod_tri_set_null",
        "fkod_tri_author",
    ] {
        drop_table(pool, table).await;
    }
    let mut models = vec![
        Author::SCHEMA,
        PostSetNull::SCHEMA,
        PostRestrict::SCHEMA,
        Reply::SCHEMA,
    ];
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

/// An author with a `SET NULL` post, a `RESTRICT` post and a reply to it.
async fn seed_referenced_author(pool: &Pool) {
    Author { id: 1 }.insert_pool(pool).await.expect("author");
    PostSetNull {
        id: 10,
        author_id: Some(1),
    }
    .insert_pool(pool)
    .await
    .expect("set null child");
    PostRestrict {
        id: 30,
        author_id: 1,
    }
    .insert_pool(pool)
    .await
    .expect("restrict child");
    Reply {
        id: 40,
        post_id: 30,
    }
    .insert_pool(pool)
    .await
    .expect("grandchild");
}

/// A plain truncate refuses while a `RESTRICT` row points at the table,
/// on every backend, and changes nothing; once it is gone, `SET NULL` runs.
async fn truncate_refuses_while_a_restricting_row_exists(pool: &Pool) {
    seed_referenced_author(pool).await;
    let err = Author::truncate(pool)
        .await
        .expect_err("a RESTRICT row must block a plain truncate");
    assert!(
        matches!(
            err,
            ExecError::TruncateReferenced {
                table: "fkod_tri_author",
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(Author::objects().count(pool).await.unwrap(), 1);
    assert_eq!(PostRestrict::objects().count(pool).await.unwrap(), 1);
    let kept = PostSetNull::objects().fetch(pool).await.expect("fetch");
    assert_eq!(kept[0].author_id, Some(1), "the refused truncate set NULL");

    Reply::truncate(pool).await.expect("leaf truncate");
    PostRestrict::truncate(pool)
        .await
        .expect("restrict child truncate");
    assert_eq!(Author::truncate(pool).await.expect("truncate"), 1);
    let nulled = PostSetNull::objects().fetch(pool).await.expect("fetch");
    assert_eq!(nulled.len(), 1, "a plain truncate follows SET NULL");
    assert_eq!(nulled[0].author_id, None);
}

/// `truncate_cascade` empties every table that references the author,
/// transitively and whatever its `ON DELETE`, on every backend.
async fn truncate_cascade_empties_every_referencing_table(pool: &Pool) {
    seed_referenced_author(pool).await;
    Author::truncate_cascade(pool).await.expect("cascade");
    assert_eq!(Author::objects().count(pool).await.unwrap(), 0);
    assert_eq!(PostSetNull::objects().count(pool).await.unwrap(), 0);
    assert_eq!(PostRestrict::objects().count(pool).await.unwrap(), 0);
    assert_eq!(Reply::objects().count(pool).await.unwrap(), 0);
    // The FK checks are back on afterwards.
    let orphan = Reply {
        id: 41,
        post_id: 99,
    }
    .insert_pool(pool)
    .await;
    assert!(orphan.is_err(), "an orphan insert passed after the cascade");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        parent_delete_runs_the_declared_action,
        truncate_refuses_while_a_restricting_row_exists,
        truncate_cascade_empties_every_referencing_table,
    ],
}
