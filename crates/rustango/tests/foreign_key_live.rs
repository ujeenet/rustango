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
            assert_eq!(pk, "9999");
        }
        other => panic!("expected ForeignKeyTargetMissing, got {other:?}"),
    }
}

// ---------- Non-i64 PK (String) ----------

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_fk_str_user", display = "name")]
pub struct StrUser {
    /// Plain `String` PK — not `Auto<…>`.
    #[rustango(primary_key, max_length = 36)]
    pub user_uuid: String,
    #[rustango(max_length = 64)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_fk_str_post")]
pub struct StrPost {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 128)]
    pub title: String,
    /// FK column carries `StrUser::user_uuid` — a `String` PK.
    #[rustango(max_length = 36, on = "user_uuid")]
    pub author: rustango::sql::ForeignKey<StrUser, String>,
}

async fn fresh_pool_str() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS rustango_fk_str_post CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS rustango_fk_str_user CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE rustango_fk_str_user (
              user_uuid VARCHAR(36) PRIMARY KEY,
              name VARCHAR(64) NOT NULL
           )"#,
    ).execute(&pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE rustango_fk_str_post (
              id BIGSERIAL PRIMARY KEY,
              title VARCHAR(128) NOT NULL,
              author VARCHAR(36) NOT NULL REFERENCES rustango_fk_str_user (user_uuid)
           )"#,
    ).execute(&pool).await.unwrap();
    Some(pool)
}

#[tokio::test]
async fn string_pk_fk_round_trips_with_lazy_load() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool_str().await else {
        return;
    };

    let alice_uuid = "alice-uuid-0000".to_owned();
    let alice = StrUser { user_uuid: alice_uuid.clone(), name: "alice".into() };
    alice.insert(&pool).await.unwrap();

    let mut post = StrPost {
        id: Auto::default(),
        title: "hello string fk".into(),
        author: ForeignKey::unloaded(alice_uuid.clone()),
    };
    post.insert(&pool).await.unwrap();

    // Round-trip through fetch — confirms FromRow decodes a String FK.
    let mut rows: Vec<StrPost> = StrPost::objects()
        .filter("id", Op::Eq, post.id)
        .fetch(&pool).await.unwrap();
    let mut fetched = rows.pop().expect("post round-trip");
    assert_eq!(fetched.author.pk(), alice_uuid);
    assert!(!fetched.author.is_loaded());

    // Lazy-load resolves the parent.
    let parent = fetched.author.get(&pool).await.unwrap();
    assert_eq!(parent.name, "alice");
    assert!(fetched.author.is_loaded());
}

#[tokio::test]
async fn string_pk_fk_missing_target_renders_pk_in_error() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool_str().await else {
        return;
    };

    let mut orphan: ForeignKey<StrUser, String> =
        ForeignKey::unloaded("does-not-exist".to_owned());
    let err = orphan.get(&pool).await.unwrap_err();
    match err {
        ExecError::ForeignKeyTargetMissing { table, pk } => {
            assert_eq!(table, "rustango_fk_str_user");
            assert_eq!(pk, "does-not-exist");
        }
        other => panic!("expected ForeignKeyTargetMissing, got {other:?}"),
    }
}

// ---------- Nullable FK (Option<ForeignKey<T>>) ----------

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_fk_nl_author", display = "name")]
pub struct NlAuthor {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_fk_nl_book")]
pub struct NlBook {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 128)]
    pub title: String,
    /// Optional FK — books may have no author.
    pub author: Option<rustango::sql::ForeignKey<NlAuthor>>,
}

async fn fresh_pool_nl() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS rustango_fk_nl_book CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS rustango_fk_nl_author CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE rustango_fk_nl_author (
              id BIGSERIAL PRIMARY KEY,
              name VARCHAR(64) NOT NULL
           )"#,
    ).execute(&pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE rustango_fk_nl_book (
              id BIGSERIAL PRIMARY KEY,
              title VARCHAR(128) NOT NULL,
              author BIGINT NULL REFERENCES rustango_fk_nl_author (id)
           )"#,
    ).execute(&pool).await.unwrap();
    Some(pool)
}

#[tokio::test]
async fn nullable_fk_round_trip_with_some_and_none() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool_nl().await else {
        return;
    };

    let mut alice = NlAuthor { id: Auto::default(), name: "alice".into() };
    alice.insert(&pool).await.unwrap();
    let alice_pk = match alice.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("alice.id should be populated"),
    };

    let mut with_author = NlBook {
        id: Auto::default(),
        title: "Authored".into(),
        author: Some(ForeignKey::unloaded(alice_pk)),
    };
    with_author.insert(&pool).await.unwrap();

    let mut anon = NlBook {
        id: Auto::default(),
        title: "Anonymous".into(),
        author: None,
    };
    anon.insert(&pool).await.unwrap();

    let rows: Vec<NlBook> = NlBook::objects()
        .order_by(&[("id", false)])
        .fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    let by_title: std::collections::HashMap<String, NlBook> =
        rows.into_iter().map(|b| (b.title.clone(), b)).collect();

    let authored = &by_title["Authored"];
    let anonymous = &by_title["Anonymous"];

    assert!(authored.author.is_some(), "Authored book should keep its FK");
    assert_eq!(
        authored.author.as_ref().unwrap().pk(),
        alice_pk,
        "FK PK round-trips through fetch"
    );
    assert!(anonymous.author.is_none(), "Anonymous book should be NULL");

    // Lazy-load on the Some branch.
    let mut authored_mut = authored.clone();
    let parent = authored_mut.author.as_mut().unwrap().get(&pool).await.unwrap();
    assert_eq!(parent.name, "alice");
}

// ---------- i16 (SMALLINT) end-to-end ----------

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_i16_status")]
pub struct StatusRow {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    /// Bounded code stored as Postgres SMALLINT.
    pub code: i16,
    pub label: String,
    /// Optional priority — covers nullable i16.
    pub priority: Option<i16>,
}

async fn fresh_pool_i16() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS rustango_i16_status CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE rustango_i16_status (
              id BIGSERIAL PRIMARY KEY,
              code SMALLINT NOT NULL,
              label VARCHAR(64) NOT NULL,
              priority SMALLINT NULL
           )"#,
    ).execute(&pool).await.unwrap();
    Some(pool)
}

#[tokio::test]
async fn i16_field_round_trips_against_smallint() {
    let _g = fk_lock().lock().await;
    let Some(pool) = fresh_pool_i16().await else {
        return;
    };

    let mut row = StatusRow {
        id: Auto::default(),
        code: 7,
        label: "draft".to_owned(),
        priority: Some(-3),
    };
    row.insert(&pool).await.unwrap();

    let mut blank = StatusRow {
        id: Auto::default(),
        code: i16::MAX,
        label: "max".to_owned(),
        priority: None,
    };
    blank.insert(&pool).await.unwrap();

    let mut neg = StatusRow {
        id: Auto::default(),
        code: i16::MIN,
        label: "min".to_owned(),
        priority: Some(0),
    };
    neg.insert(&pool).await.unwrap();

    let rows: Vec<StatusRow> = StatusRow::objects()
        .order_by(&[("id", false)])
        .fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    let by_label: std::collections::HashMap<String, StatusRow> =
        rows.into_iter().map(|r| (r.label.clone(), r)).collect();
    assert_eq!(by_label["draft"].code, 7);
    assert_eq!(by_label["draft"].priority, Some(-3));
    assert_eq!(by_label["max"].code, i16::MAX);
    assert_eq!(by_label["max"].priority, None);
    assert_eq!(by_label["min"].code, i16::MIN);
}
