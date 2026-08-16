//! `#[rustango(soft_delete)]` has to compile when the `postgres` feature is
//! **off**.
//!
//! This file is the regression guard for a bug that three existing sqlite
//! soft-delete suites could not catch. They are `cfg(feature = "sqlite")`, and
//! the crate's default features include `postgres` — so every one of them ran
//! with the PG backend also compiled in, which is exactly the condition that
//! hid the fault.
//!
//! The derive emits `soft_delete_on` / `restore_on`, which take a Postgres
//! executor and reach `sql::__macro_internals`. That module is
//! `#[cfg(feature = "postgres")]`. The two methods were the only PG-executor
//! methods in the macro missing the same gate, so the moment a *consumer*
//! built sqlite-only — as a dialect-native app does — the derive expanded to a
//! reference to a module that was configured out, and the model would not
//! compile at all.
//!
//! Hence the inverted gate below: this file exists only in the build the other
//! suites cannot see. Run it with
//! `cargo test -p rustango --no-default-features --features sqlite,testkit
//! --test soft_delete_without_postgres`.

#![cfg(all(feature = "sqlite", not(feature = "postgres")))]

use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sdnp_note")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn pool() -> Pool {
    let path = std::env::temp_dir().join("rustango_sd_no_pg.sqlite");
    let _ = std::fs::remove_file(&path);
    let pool = Pool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("connect");
    rustango::testkit::create_tables_for::<Note>(&pool)
        .await
        .expect("schema");
    pool
}

async fn a_note(pool: &Pool, title: &str) -> Note {
    let mut n = Note {
        id: Auto::Unset,
        title: title.to_owned(),
        deleted_at: None,
    };
    n.insert_pool(pool).await.expect("insert");
    n
}

/// The compile is most of the test — if this file builds at all, the gate is
/// right. The assertions confirm the pool-based trio is what remains, and that
/// it actually works without a Postgres backend anywhere in the build.
#[tokio::test]
async fn the_pool_trio_works_with_no_postgres_backend() {
    let pool = pool().await;
    let keep = a_note(&pool, "keep").await;
    let gone = a_note(&pool, "gone").await;

    gone.soft_delete(&pool).await.expect("soft delete");

    let live: Vec<Note> = Note::objects().active().fetch(&pool).await.expect("active");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id.get().copied(), keep.id.get().copied());

    let trashed: Vec<Note> = Note::objects()
        .only_trashed()
        .fetch(&pool)
        .await
        .expect("only_trashed");
    assert_eq!(trashed.len(), 1, "the row is still there, marked");

    gone.restore(&pool).await.expect("restore");
    assert_eq!(
        Note::objects()
            .active()
            .fetch(&pool)
            .await
            .expect("active")
            .len(),
        2,
        "restore puts it back"
    );

    gone.force_delete(&pool).await.expect("force delete");
    assert_eq!(
        Note::objects()
            .with_trashed()
            .fetch(&pool)
            .await
            .expect("all")
            .len(),
        1,
        "force_delete really removes the row"
    );
}
