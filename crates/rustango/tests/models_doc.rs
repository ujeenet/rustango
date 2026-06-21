//! Backing test for `docs/models.md` — proves the headline claims of the Models
//! reference on in-memory SQLite: field-type round-trips, the default `Auto<i64>`
//! primary key, a custom application-assigned `String` PK, a custom PK column
//! name, choices/defaults/auto_now_add/soft-delete, and `Model::SCHEMA`
//! introspection.
//!
//! Run: `cargo test -p rustango --features sqlite --test models_doc`

#![cfg(feature = "sqlite")]
#![allow(irrefutable_let_patterns)]

use chrono::{DateTime, Utc};
use rustango::core::Model as _; // brings `T::SCHEMA` into scope
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

// ---------------------------------------------------------------- field types

#[derive(Model, Debug, Clone)]
#[rustango(table = "md_gadget")]
pub struct Gadget {
    #[rustango(primary_key)]
    pub id: Auto<i64>, // default PK: auto-increment, server-assigned
    #[rustango(max_length = 100)]
    pub name: String,
    pub qty: i64,
    pub active: bool,
    pub note: Option<String>, // nullable
    pub made_at: DateTime<Utc>,
    pub meta: serde_json::Value,
}

async fn gadget_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query(
        "CREATE TABLE md_gadget (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            name    TEXT NOT NULL,
            qty     INTEGER NOT NULL,
            active  INTEGER NOT NULL,
            note    TEXT,
            made_at TEXT NOT NULL,
            meta    TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn field_types_round_trip() {
    let pool = gadget_pool().await;

    let mut g = Gadget {
        id: Auto::default(), // Unset → the DB assigns it on insert
        name: "Sprocket".into(),
        qty: 7,
        active: true,
        note: None,
        made_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc),
        meta: serde_json::json!({ "color": "red", "tags": [1, 2, 3] }),
    };
    g.save_pool(&pool).await.unwrap();

    // The PK was populated by the database on insert.
    let id = g.id.get().copied().expect("id assigned");
    assert!(id >= 1);

    // Read it back — every field type round-trips.
    let back = Gadget::find_or_fail(id, &pool).await.unwrap();
    assert_eq!(back.name, "Sprocket");
    assert_eq!(back.qty, 7);
    assert!(back.active);
    assert_eq!(back.note, None);
    assert_eq!(back.made_at, g.made_at);
    assert_eq!(back.meta["color"], "red");
}

// ---------------------------------------------------------- custom String PK

#[derive(Model, Debug, Clone)]
#[rustango(table = "md_coupon")]
pub struct Coupon {
    #[rustango(primary_key, max_length = 32)]
    pub code: String, // application-assigned PK (not Auto)
    pub discount: i64,
}

#[tokio::test]
async fn custom_string_primary_key() {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query("CREATE TABLE md_coupon ( code TEXT PRIMARY KEY, discount INTEGER NOT NULL )")
        .execute(&p)
        .await
        .unwrap();
    let pool = Pool::Sqlite(p);

    let c = Coupon {
        code: "SAVE10".into(),
        discount: 10,
    };
    // An application-assigned PK has no `Auto::Unset` state, so `save_pool`
    // would UPDATE. Use `insert_pool` to insert a brand-new row.
    c.insert_pool(&pool).await.unwrap();

    // Look up by the string PK.
    let back = Coupon::find_or_fail("SAVE10".to_string(), &pool)
        .await
        .unwrap();
    assert_eq!(back.discount, 10);

    // The schema reports the PK is the `code` column.
    let pk = Coupon::SCHEMA.primary_key().expect("has a pk");
    assert_eq!(pk.name, "code");
    assert_eq!(pk.column, "code");
}

// ------------------------------------------------------ custom PK column name

#[derive(Model, Debug, Clone)]
#[rustango(table = "md_account")]
pub struct Account {
    #[rustango(primary_key, column = "account_no")]
    pub number: i64,
    #[rustango(max_length = 100)]
    pub holder: String,
}

#[tokio::test]
async fn custom_primary_key_column_name() {
    // The Rust field is `number`; the SQL column is `account_no`.
    let pk = Account::SCHEMA.primary_key().expect("has a pk");
    assert_eq!(pk.name, "number");
    assert_eq!(pk.column, "account_no");
    assert_eq!(Account::SCHEMA.table, "md_account");
}

// ------------------------------- choices + default + auto_now_add + soft_delete

#[derive(Model, Debug, Clone)]
#[rustango(table = "md_article")]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(
        max_length = 20,
        default = "'draft'",
        choices = "draft:Draft, published:Published"
    )]
    pub status: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>, // server-set on insert
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

async fn article_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query(
        "CREATE TABLE md_article (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            title      TEXT NOT NULL,
            status     TEXT NOT NULL DEFAULT 'draft',
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
            deleted_at TEXT
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn auto_now_add_and_soft_delete() {
    let pool = article_pool().await;

    let mut a = Article {
        id: Auto::default(),
        title: "Hello".into(),
        status: "published".into(),
        created_at: Auto::default(), // auto_now_add → server fills it
        deleted_at: None,
    };
    a.save_pool(&pool).await.unwrap();
    let id = a.id.get().copied().unwrap();

    let row = Article::find_or_fail(id, &pool).await.unwrap();
    assert!(
        row.created_at.get().is_some(),
        "auto_now_add populated created_at"
    );
    assert!(row.deleted_at.is_none());

    // soft_delete marks the row trashed; active() hides it.
    let affected = row.soft_delete(&pool).await.unwrap();
    assert_eq!(affected, 1);

    let active: Vec<Article> = Article::objects().active().fetch(&pool).await.unwrap();
    assert!(
        active.is_empty(),
        "soft-deleted row is hidden from active()"
    );

    let all: Vec<Article> = Article::objects()
        .with_trashed()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "still in the table, visible with_trashed()");
}
