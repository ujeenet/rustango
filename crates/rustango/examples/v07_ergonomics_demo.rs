//! Walks through the four v0.7 ergonomic additions in ~80 lines:
//! `save()`, `ForeignKey<T>` lazy-load, OR / nested filters, and
//! per-app migration ledger naming via `migrate::Builder`.
//!
//! Wraps everything in a single transaction-like sequence against a
//! fresh-each-run pair of tables; safe to re-run.
//!
//! # Run
//! Postgres up (`docker compose up -d` from the repo root), then:
//!
//! ```text
//! cargo run --example v07_ergonomics_demo
//! ```
//!
//! Set `DATABASE_URL` to point at your Postgres, e.g.
//! `postgres://rustango:rustango@localhost:5432/rustango_test`.

use rustango::core::Column as _;
use rustango::migrate;
use rustango::sql::sqlx::{self, PgPool};
use rustango::sql::{Auto, Fetcher, ForeignKey};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v07_demo_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub active: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "v07_demo_book")]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 128)]
    pub title: String,
    pub year: i32,
    pub author: ForeignKey<Author>, // BIGINT REFERENCES v07_demo_author(id)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgres://rustango:rustango@localhost:5432/rustango_test".into()
        });
    let pool = PgPool::connect(&url).await?;

    // Fresh-each-run tables. Drop child first because of the FK.
    for sql in [
        "DROP TABLE IF EXISTS v07_demo_book",
        "DROP TABLE IF EXISTS v07_demo_author",
    ] {
        sqlx::query(sql).execute(&pool).await?;
    }
    migrate::apply_all(&pool).await?;

    // ──────────────────────────────────────────────────────────────
    // 1) save() — Auto::Unset → INSERT, then Auto::Set(_) → UPDATE.
    // ──────────────────────────────────────────────────────────────
    let mut alice = Author {
        id: Auto::default(),
        name: "alice".into(),
        active: true,
    };
    alice.save(&pool).await?; // INSERT (PK was Unset; populated by RETURNING)
    println!("alice INSERTed, id = {:?}", alice.id);
    alice.active = false;
    alice.save(&pool).await?; // UPDATE (PK is now Set; same row)
    println!("alice UPDATEd to inactive");

    let mut bob = Author {
        id: Auto::default(),
        name: "bob".into(),
        active: true,
    };
    bob.save(&pool).await?;

    // ──────────────────────────────────────────────────────────────
    // 2) ForeignKey<T> lazy-load.
    // ──────────────────────────────────────────────────────────────
    let mut hello = Book {
        id: Auto::default(),
        title: "Hello, World".into(),
        year: 2020,
        author: ForeignKey::unloaded(bob.id.get().copied().unwrap()),
    };
    hello.save(&pool).await?;

    // Re-fetch from the DB so the FK lands as Unloaded(pk).
    let mut fetched = Book::objects()
        .where_(Book::id.eq(*hello.id.get().unwrap()))
        .fetch(&pool)
        .await?
        .pop()
        .unwrap();
    println!("fetched book.author state (pre-get): {:?}", fetched.author);
    let resolved: &Author = fetched.author.get(&pool).await?;
    println!(
        "lazy-loaded fetched.author = {:?} (cached on the field; second .get() is no-SQL)",
        resolved.name
    );

    // ──────────────────────────────────────────────────────────────
    // 3) OR / nested-expr filters.
    // ──────────────────────────────────────────────────────────────
    let mut carol = Author {
        id: Auto::default(),
        name: "carol".into(),
        active: false,
    };
    carol.save(&pool).await?;

    // (name = "alice" OR name = "bob") AND active = false
    let inactive_named: Vec<Author> = Author::objects()
        .where_(Author::name.eq("alice").or(Author::name.eq("bob")))
        .where_(Author::active.eq(false))
        .fetch(&pool)
        .await?;
    println!(
        "(alice OR bob) AND inactive → {:?}",
        inactive_named.iter().map(|a| &a.name).collect::<Vec<_>>()
    );

    // Nested: active = true OR (name = "carol" AND id > 0)
    let mixed: Vec<Author> = Author::objects()
        .where_(
            Author::active
                .eq(true)
                .or(Author::name.eq("carol").and(Author::id.gt(0_i64))),
        )
        .fetch(&pool)
        .await?;
    println!(
        "active OR (carol AND id > 0) → {:?}",
        mixed.iter().map(|a| &a.name).collect::<Vec<_>>()
    );

    // ──────────────────────────────────────────────────────────────
    // 4) Per-app migration ledger naming via migrate::Builder.
    //    Two apps in one DB → two ledger tables.
    // ──────────────────────────────────────────────────────────────
    let app_a = migrate::Builder::new().ledger("__v07_demo_app_a__");
    let app_b = migrate::Builder::new().ledger("__v07_demo_app_b__");
    app_a.ensure_ledger(&pool).await?;
    app_b.ensure_ledger(&pool).await?;
    println!(
        "ledger names → app_a: {:?}, app_b: {:?} — two distinct bookkeeping tables in one DB",
        app_a.ledger_name(),
        app_b.ledger_name()
    );

    // Cleanup.
    for sql in [
        "DROP TABLE IF EXISTS v07_demo_book",
        "DROP TABLE IF EXISTS v07_demo_author",
        "DROP TABLE IF EXISTS __v07_demo_app_a__",
        "DROP TABLE IF EXISTS __v07_demo_app_b__",
    ] {
        sqlx::query(sql).execute(&pool).await?;
    }

    Ok(())
}
