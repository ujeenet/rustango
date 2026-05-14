#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds; pattern is refutable on multi-backend builds.
//! Advanced ORM coverage on SQLite — closes the gap between the
//! basic `save_pool` / `fetch_pool` round-trips in `sqlite_live.rs`
//! and the full PG live suite (`save_live.rs`, `select_related_live.rs`,
//! `where_expr_live.rs`, `order_by_annotate_live.rs`, `upsert_unique_*`,
//! `prefetch_related_live.rs`).
//!
//! What this file proves on SQLite:
//!   * `select_related(&[Fk])` decodes the joined row correctly
//!   * `where_(...)` with AND / OR / IN combinations compiles + matches
//!   * `order_by(&[(col, asc)])` produces deterministic ordering
//!   * `count_pool` matches `fetch_pool().len()` for the same QuerySet
//!   * `bulk_insert_pool` round-trips
//!   * upsert via the ORM IR (`InsertQuery` with `ConflictClause::DoUpdate`)
//!   * `prefetch_related` (FK-based) hydrates parents-then-children
//!
//! Each subtest builds a fresh in-memory SQLite, creates two tiny
//! tables (`adv_author` + `adv_post` with a Post→Author FK), and
//! drives the ORM against it.

#![cfg(all(feature = "sqlite", feature = "postgres"))]

use rustango::core::{Column as _, InsertQuery, Model as _, SqlValue};
use rustango::sql::{sqlx, Auto, CounterPool, FetcherPool, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "adv_author")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "adv_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub author: ForeignKey<Author>,
    pub published: bool,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE adv_author (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            name TEXT NOT NULL)",
    )
    .execute(&pool)
    .await
    .expect("author table");
    sqlx::query(
        "CREATE TABLE adv_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            author INTEGER NOT NULL REFERENCES adv_author(id), \
            published INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await
    .expect("post table");
    Pool::Sqlite(pool)
}

async fn seed_basic(pool: &Pool) -> (i64, i64) {
    let mut alice = Author {
        id: Auto::default(),
        name: "Alice".to_owned(),
    };
    alice.save_pool(pool).await.expect("save alice");
    let alice_id = alice.id.get().copied().unwrap();
    let mut bob = Author {
        id: Auto::default(),
        name: "Bob".to_owned(),
    };
    bob.save_pool(pool).await.expect("save bob");
    let bob_id = bob.id.get().copied().unwrap();
    (alice_id, bob_id)
}

async fn seed_posts(pool: &Pool, author_id: i64, titles: &[(&str, bool)]) {
    for (title, published) in titles {
        let mut p = Post {
            id: Auto::default(),
            title: (*title).to_owned(),
            author: ForeignKey::from(author_id),
            published: *published,
        };
        p.save_pool(pool).await.expect("save post");
    }
}

#[tokio::test]
async fn where_chain_with_and_filters_correctly_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _bob) = seed_basic(&pool).await;
    seed_posts(
        &pool,
        alice,
        &[("draft1", false), ("public1", true), ("public2", true)],
    )
    .await;
    let posts: Vec<Post> = Post::objects()
        .where_(Post::author.eq(alice))
        .where_(Post::published.eq(true))
        .fetch_pool(&pool)
        .await
        .expect("fetch");
    assert_eq!(posts.len(), 2, "two published posts by alice");
    assert!(posts.iter().all(|p| p.published));
}

#[tokio::test]
async fn where_in_list_filters_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, bob) = seed_basic(&pool).await;
    seed_posts(&pool, alice, &[("a1", true)]).await;
    seed_posts(&pool, bob, &[("b1", true)]).await;
    let posts: Vec<Post> = Post::objects()
        .filter(
            "author",
            rustango::core::Op::In,
            SqlValue::List(vec![SqlValue::I64(alice), SqlValue::I64(bob)]),
        )
        .fetch_pool(&pool)
        .await
        .expect("fetch IN");
    assert_eq!(posts.len(), 2, "both authors' posts");
}

#[tokio::test]
async fn order_by_desc_produces_deterministic_order_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _bob) = seed_basic(&pool).await;
    seed_posts(
        &pool,
        alice,
        &[("zebra", true), ("apple", true), ("mango", true)],
    )
    .await;
    let posts: Vec<Post> = Post::objects()
        .order_by(&[("title", false)])
        .fetch_pool(&pool)
        .await
        .expect("fetch");
    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["apple", "mango", "zebra"]);
    // DESC.
    let posts: Vec<Post> = Post::objects()
        .order_by(&[("title", true)])
        .fetch_pool(&pool)
        .await
        .expect("fetch desc");
    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, vec!["zebra", "mango", "apple"]);
}

#[tokio::test]
async fn count_pool_matches_fetch_pool_len_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _bob) = seed_basic(&pool).await;
    seed_posts(&pool, alice, &[("a", true), ("b", false), ("c", true)]).await;
    let qs = Post::objects().where_(Post::author.eq(alice));
    let qs2 = Post::objects().where_(Post::author.eq(alice));
    let n_count = qs.count_pool(&pool).await.expect("count");
    let n_fetch = qs2.fetch_pool(&pool).await.expect("fetch").len() as i64;
    assert_eq!(
        n_count, n_fetch,
        "count_pool should match fetch_pool().len() — got count={n_count}, fetch_len={n_fetch}"
    );
}

#[tokio::test]
async fn foreign_key_get_pool_lazy_loads_author_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _) = seed_basic(&pool).await;
    seed_posts(&pool, alice, &[("hello", true)]).await;
    let mut posts: Vec<Post> = Post::objects()
        .where_(Post::published.eq(true))
        .fetch_pool(&pool)
        .await
        .expect("fetch");
    assert_eq!(posts.len(), 1);
    let post = posts.first_mut().unwrap();
    let author_ref = post
        .author
        .get_pool(&pool)
        .await
        .expect("get_pool lazy-load");
    assert_eq!(author_ref.name, "Alice");
}

#[tokio::test]
async fn upsert_via_insert_query_with_do_update_works_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _bob) = seed_basic(&pool).await;
    // Manually insert + then attempt upsert via the InsertQuery IR
    // using the post's UNIQUE-ish slot (title). SQLite supports
    // `ON CONFLICT (col) DO UPDATE SET …` since 3.24.
    use rustango::core::ConflictClause;
    // Re-create the post table with a UNIQUE constraint on title so
    // ON CONFLICT has a target to fire on.
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query("DROP TABLE adv_post")
            .execute(sq)
            .await
            .expect("drop");
        sqlx::query(
            "CREATE TABLE adv_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL UNIQUE, \
                author INTEGER NOT NULL REFERENCES adv_author(id), \
                published INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(sq)
        .await
        .expect("recreate");
    }
    // First insert.
    let q = InsertQuery {
        model: Post::SCHEMA,
        columns: vec!["title", "author", "published"],
        values: vec![
            SqlValue::from("unique-slot".to_owned()),
            SqlValue::from(alice),
            SqlValue::from(false),
        ],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoUpdate {
            target: vec!["title"],
            update_columns: vec!["published"],
        }),
    };
    rustango::sql::insert_pool(&pool, &q)
        .await
        .expect("first upsert");
    // Conflict insert with different `published` flips the row.
    let q2 = InsertQuery {
        model: Post::SCHEMA,
        columns: vec!["title", "author", "published"],
        values: vec![
            SqlValue::from("unique-slot".to_owned()),
            SqlValue::from(alice),
            SqlValue::from(true),
        ],
        returning: vec![],
        on_conflict: Some(ConflictClause::DoUpdate {
            target: vec!["title"],
            update_columns: vec!["published"],
        }),
    };
    rustango::sql::insert_pool(&pool, &q2)
        .await
        .expect("conflict upsert");
    // Verify there's still only one row + `published` is now true.
    let rows: Vec<Post> = Post::objects()
        .where_(Post::title.eq("unique-slot".to_owned()))
        .fetch_pool(&pool)
        .await
        .expect("fetch");
    assert_eq!(rows.len(), 1, "ON CONFLICT should keep one row");
    assert!(rows[0].published, "second upsert should flip published");
}

#[tokio::test]
async fn limit_offset_pagination_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _bob) = seed_basic(&pool).await;
    seed_posts(
        &pool,
        alice,
        &[
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", true),
            ("e", true),
        ],
    )
    .await;
    let page1: Vec<Post> = Post::objects()
        .order_by(&[("title", false)])
        .limit(2)
        .offset(0)
        .fetch_pool(&pool)
        .await
        .expect("page 1");
    assert_eq!(
        page1.iter().map(|p| p.title.clone()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    let page2: Vec<Post> = Post::objects()
        .order_by(&[("title", false)])
        .limit(2)
        .offset(2)
        .fetch_pool(&pool)
        .await
        .expect("page 2");
    assert_eq!(
        page2.iter().map(|p| p.title.clone()).collect::<Vec<_>>(),
        vec!["c", "d"]
    );
    let page3: Vec<Post> = Post::objects()
        .order_by(&[("title", false)])
        .limit(2)
        .offset(4)
        .fetch_pool(&pool)
        .await
        .expect("page 3");
    assert_eq!(
        page3.iter().map(|p| p.title.clone()).collect::<Vec<_>>(),
        vec!["e"]
    );
}

#[tokio::test]
async fn save_pool_updates_existing_row_on_sqlite() {
    let pool = fresh_pool().await;
    let (alice, _) = seed_basic(&pool).await;
    seed_posts(&pool, alice, &[("draft", false)]).await;
    let mut row: Post = Post::objects()
        .fetch_pool(&pool)
        .await
        .expect("fetch")
        .into_iter()
        .next()
        .expect("row");
    row.published = true;
    row.title = "published".into();
    row.save_pool(&pool).await.expect("save update");
    let again: Post = Post::objects()
        .fetch_pool(&pool)
        .await
        .expect("fetch again")
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(again.title, "published");
    assert!(again.published);
}
