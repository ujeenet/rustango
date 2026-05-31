//! Django-parity #375 — `ModelForm::prepare_save()` returns a
//! mutable `PreparedSave` (Django's `form.save(commit=False)`
//! shape) the caller can mutate before
//! `PreparedSave::commit_pool` actually runs the INSERT.
//!
//! Validates the canonical use case: the form omits a column the
//! DB requires (because it's derived from the session, not the
//! POST body) — the view layer fills it in via `.set(...)` between
//! prepare + commit.

#![cfg(feature = "sqlite")]

use std::collections::HashMap;

use rustango::core::Model as _;
use rustango::core::SqlValue;
use rustango::forms::ModelForm;
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ps_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub title: String,
    /// Session-derived — never appears in the public form.
    pub author_id: i64,
}

/// Extract the underlying `&SqlitePool` from a `Pool` enum so we
/// can run hand-rolled SELECTs through sqlx without bouncing off
/// the framework's tri-dialect helpers — the test only runs under
/// `cfg(feature = "sqlite")`, but the compiler still wants every
/// arm of the `Pool` enum handled. An `#[allow(irrefutable_let_patterns)]`
/// here would be slightly more honest but `if let` is cleaner.
fn sqlite_pool(pool: &Pool) -> &sqlx::SqlitePool {
    #[allow(irrefutable_let_patterns)]
    if let Pool::Sqlite(s) = pool {
        s
    } else {
        unreachable!("test gated to cfg(feature = sqlite)")
    }
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE "ps_post" (
            "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"     TEXT NOT NULL,
            "author_id" INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

#[tokio::test]
async fn prepare_save_then_set_session_field_then_commit_inserts_row() {
    let pool = fresh_pool().await;

    // Form arrives from the request body — `author_id` is absent
    // because the public form never collects it.
    let mut post_body: HashMap<String, String> = HashMap::new();
    post_body.insert("title".into(), "Late binding wins".into());

    let form = ModelForm::new(Post::SCHEMA, post_body).exclude(&["author_id"]);
    let mut prep = form.prepare_save().expect("valid");
    assert!(prep.is_insert(), "no pk supplied → INSERT");
    assert!(!prep.has("author_id"), "session field intentionally absent");

    // View layer adds the session-derived field before commit —
    // the same shape as Django's `obj = form.save(commit=False); obj.author = request.user; obj.save()`.
    prep.set("author_id", SqlValue::I64(42));
    assert!(prep.has("author_id"));

    let pk = prep.commit_pool(&pool).await.expect("insert");
    assert!(matches!(pk, SqlValue::I64(_)));

    // Round-trip — the row landed with the session-derived author_id.
    let count: i64 = sqlx::query_scalar::<_, i64>("SELECT author_id FROM ps_post WHERE title = ?")
        .bind("Late binding wins")
        .fetch_one(sqlite_pool(&pool))
        .await
        .expect("fetch");
    assert_eq!(count, 42);
}

#[tokio::test]
async fn prepare_save_then_unset_drops_field_from_insert() {
    let pool = fresh_pool().await;

    // Form has all fields, but the view wants to drop title before
    // commit (contrived but mirrors `del obj.title` between
    // `save(commit=False)` and `obj.save()`). title is NOT NULL in
    // the table — commit should fail at the DB layer.
    let mut post_body: HashMap<String, String> = HashMap::new();
    post_body.insert("title".into(), "to-be-dropped".into());
    post_body.insert("author_id".into(), "7".into());

    let form = ModelForm::new(Post::SCHEMA, post_body);
    let mut prep = form.prepare_save().expect("valid");
    assert!(prep.has("title"));
    prep.unset("title");
    assert!(!prep.has("title"), "unset() should remove title");

    let result = prep.commit_pool(&pool).await;
    assert!(
        result.is_err(),
        "commit without NOT NULL title should fail at the DB, got {result:?}"
    );
}

#[tokio::test]
async fn prepare_save_updates_existing_row_with_overridden_value() {
    let pool = fresh_pool().await;

    // Seed a row to update.
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO ps_post (id, title, author_id) VALUES (1, 'original', 7)"#,
        Vec::new(),
    )
    .await
    .expect("seed");

    // Form data carries the new title; view-side override wins
    // for author_id (e.g. recording the editor instead of the
    // original author).
    let mut post_body: HashMap<String, String> = HashMap::new();
    post_body.insert("title".into(), "form-edit".into());
    post_body.insert("author_id".into(), "7".into());

    let form = ModelForm::for_update(Post::SCHEMA, post_body, SqlValue::I64(1));
    let mut prep = form.prepare_save().expect("valid");
    assert!(!prep.is_insert(), "pk_value supplied → UPDATE");
    prep.set("author_id", SqlValue::I64(99));

    let pk = prep.commit_pool(&pool).await.expect("update");
    assert_eq!(pk, SqlValue::I64(1));

    let (title, author): (String, i64) =
        sqlx::query_as::<_, (String, i64)>("SELECT title, author_id FROM ps_post WHERE id = 1")
            .fetch_one(match &pool {
                Pool::Sqlite(s) => s,
                _ => unreachable!(),
            })
            .await
            .expect("fetch");
    assert_eq!(title, "form-edit", "form's title should land");
    assert_eq!(author, 99, "view-side override should win over form value");
}
