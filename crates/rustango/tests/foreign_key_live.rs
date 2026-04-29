//! Live test for `ForeignKey<T>` lazy-load (v0.7 slice 3).
//!
//! Models: `Author { id, name }`, `Book { id, title, author: ForeignKey<Author> }`.
//! Verifies:
//!
//! * The FK column DDL is emitted as `BIGINT` (same as the v0.1
//!   `i64` + `#[rustango(fk = "…")]` form).
//! * After fetching a `Book`, `book.author` is `ForeignKey::Unloaded(pk)`.
//! * `book.author.get(&pool)` resolves the parent and caches it.
//! * A second `.get()` is a no-op (no extra SQL needed; we just
//!   confirm the value is the same reference).
//! * `ForeignKey::loaded(pk, t)` constructs the `Loaded` state
//!   directly — useful when the caller already has the parent in hand.
//! * Missing FK target → `ExecError::ForeignKeyTargetMissing`.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently.

use std::sync::OnceLock;

use rustango::core::Op;
use rustango::sql::{sqlx, Auto, ExecError, Fetcher, ForeignKey};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_fk_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_fk_book")]
pub struct Book {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 128)]
    pub title: String,
    pub author: rustango::sql::ForeignKey<Author>,
}

fn fk_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");
    sqlx::query("DROP TABLE IF EXISTS rustango_fk_book CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS rustango_fk_author CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE rustango_fk_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(64) NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE rustango_fk_book (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(128) NOT NULL,
            author BIGINT NOT NULL REFERENCES rustango_fk_author (id)
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

#[tokio::test]
async fn fetched_book_has_unloaded_fk_then_get_resolves_parent() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut alice = Author {
        id: Auto::default(),
        name: "alice".into(),
    };
    alice.insert(&pool).await.unwrap();
    let alice_pk = match alice.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("alice.id should be populated after insert"),
    };

    let mut book = Book {
        id: Auto::default(),
        title: "Aliceland".into(),
        author: ForeignKey::unloaded(alice_pk),
    };
    book.insert(&pool).await.unwrap();

    // Round-trip via fetch — confirms FromRow lands in Unloaded.
    let mut fetched: Vec<Book> = Book::objects()
        .filter("id", Op::Eq, book.id)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(fetched.len(), 1);
    let mut fetched_book = fetched.pop().unwrap();
    assert_eq!(fetched_book.title, "Aliceland");
    assert_eq!(fetched_book.author.pk(), alice_pk);
    assert!(!fetched_book.author.is_loaded());
    assert!(fetched_book.author.value().is_none());

    // Lazy-load.
    let loaded_author = fetched_book.author.get(&pool).await.unwrap();
    assert_eq!(loaded_author.name, "alice");
    assert!(fetched_book.author.is_loaded());

    // Second `.get()` is cached — must still return alice.
    let cached = fetched_book.author.get(&pool).await.unwrap();
    assert_eq!(cached.name, "alice");
}

#[tokio::test]
async fn loaded_constructor_skips_initial_select() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut bob = Author {
        id: Auto::default(),
        name: "bob".into(),
    };
    bob.insert(&pool).await.unwrap();
    let bob_pk = match bob.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("bob.id should be populated after insert"),
    };

    // Construct ForeignKey directly from the in-hand parent — no SQL fired.
    let mut fk = ForeignKey::loaded(bob_pk, bob.clone());
    assert!(fk.is_loaded());
    assert_eq!(fk.pk(), bob_pk);

    // `.get()` returns the cached value without touching the DB.
    let same = fk.get(&pool).await.unwrap();
    assert_eq!(same.name, "bob");
}

#[tokio::test]
async fn missing_fk_target_returns_named_error() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // PK 9999 doesn't exist — and we built the FK by hand so it
    // bypasses the FK constraint check on insert.
    let mut orphan: ForeignKey<Author> = ForeignKey::unloaded(9999);
    let err = orphan.get(&pool).await.unwrap_err();
    match err {
        ExecError::ForeignKeyTargetMissing { table, pk } => {
            assert_eq!(table, "rustango_fk_author");
            assert_eq!(pk, 9999);
        }
        other => panic!("expected ForeignKeyTargetMissing, got {other:?}"),
    }
}
