//! Cookbook Chapter 2 — model attribute surface, exercised live.
//!
//! Each test corresponds 1:1 to a `### N.M` section in COOKBOOK.md.
//! Skips silently if `DATABASE_URL` is unset so `cargo test` works
//! without docker. Uses isolated table names (`cookbook_*`) so it
//! coexists with other tests and other rustango integration tests.
//!
//! Models live in `cookbook_blog::apps::blog::models`. Each test
//! drops + recreates its tables so reruns are deterministic.

use cookbook_blog::apps::blog::models::*;
use rustango::core::{Model as _, Op};
use rustango::sql::{sqlx, Auto, Fetcher};

fn url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = url()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect to docker pg"))
}

async fn fresh_schema(pool: &sqlx::PgPool) {
    // Drop in dependency-safe order.
    for ddl in [
        "DROP TABLE IF EXISTS cookbook_post CASCADE",
        "DROP TABLE IF EXISTS cookbook_rating CASCADE",
        "DROP TABLE IF EXISTS cookbook_author CASCADE",
    ] {
        sqlx::query(ddl).execute(pool).await.expect(ddl);
    }
    // Mirror what migrate would emit. Hand-rolled here so the test
    // doesn't drag in the migration runner — Chapter 4 covers that.
    sqlx::query(
        r#"CREATE TABLE cookbook_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            email VARCHAR(200) NOT NULL UNIQUE,
            bio VARCHAR(500) NULL,
            joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    ).execute(pool).await.expect("create author");

    sqlx::query(
        r#"CREATE TABLE cookbook_rating (
            id BIGSERIAL PRIMARY KEY,
            score BIGINT NOT NULL CHECK (score >= 1 AND score <= 5)
        )"#,
    ).execute(pool).await.expect("create rating");

    sqlx::query(
        r#"CREATE TABLE cookbook_post (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(200) NOT NULL,
            slug VARCHAR(200) NOT NULL UNIQUE,
            body TEXT NOT NULL,
            author_id BIGINT NOT NULL REFERENCES cookbook_author(id),
            published BOOLEAN NOT NULL DEFAULT false,
            view_count BIGINT NOT NULL,
            metadata JSONB NOT NULL,
            published_at TIMESTAMPTZ NULL
        )"#,
    ).execute(pool).await.expect("create post");

    sqlx::query("CREATE INDEX cookbook_post_author_idx ON cookbook_post(author_id)")
        .execute(pool).await.expect("create index");
}

// §2.11 / §2.12 — derive Model + Auto<i64> assigns id on save.
#[tokio::test]
async fn save_assigns_auto_pk() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "ada".into(),
        email: "ada@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.expect("save author");
    let id = match a.id { Auto::Set(v) => v, Auto::Unset => panic!("Auto<i64> never assigned") };
    assert!(id > 0, "Auto::Set value must be a real serial id, got {id}");
}

// §2.13 — Option<T> writes NULL when None and round-trips.
#[tokio::test]
async fn option_field_round_trips_null() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "cat".into(),
        email: "cat@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();

    let id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };
    let row: Vec<Author> = Author::objects()
        .filter("id", Op::Eq, id)
        .fetch(&pool).await.unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].bio, None, "Option<String>::None should round-trip as NULL");
}

// §2.14 / §2.29 — auto_now_add fills joined_at via DB DEFAULT NOW().
#[tokio::test]
async fn auto_now_add_assigns_at_insert() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "noé".into(),
        email: "noe@example.com".into(),
        bio: Some("hi".into()),
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let row: Vec<Author> = Author::objects()
        .filter("id", Op::Eq, id)
        .fetch(&pool).await.unwrap();
    let joined = match row[0].joined_at {
        Auto::Set(t) => t,
        Auto::Unset => panic!("auto_now_add did not populate joined_at"),
    };
    let drift = (chrono::Utc::now() - joined).num_seconds().abs();
    assert!(drift < 60, "joined_at should be ~now, drifted {drift}s");
}

// §2.15 — UNIQUE on email rejects duplicate insert at the DB level.
#[tokio::test]
async fn unique_constraint_rejects_duplicates() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "alice".into(),
        email: "dup@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();

    let mut b = Author {
        id: Auto::Unset,
        name: "alice2".into(),
        email: "dup@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    let err = b.save(&pool).await.expect_err("duplicate email must fail");
    assert!(
        format!("{err:?}").to_lowercase().contains("unique") ||
        format!("{err:?}").to_lowercase().contains("duplicate"),
        "expected a unique-violation, got: {err:?}",
    );
}

// §2.16 — min/max → CHECK rejects out-of-range scores.
#[tokio::test]
async fn min_max_check_rejects_out_of_range() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut ok = Rating { id: Auto::Unset, score: 3 };
    ok.save(&pool).await.expect("score 3 is valid");

    let mut bad = Rating { id: Auto::Unset, score: 99 };
    let err = bad.save(&pool).await.expect_err("score 99 violates max=5");
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        // Defense in depth: rustango client-validates against `min`/`max`
        // before the round-trip (`OutOfRange`), AND the DB CHECK
        // constraint backs it up. Either is a valid rejection.
        msg.contains("outofrange") || msg.contains("out_of_range") || msg.contains("check"),
        "expected client OutOfRange or DB CHECK violation, got: {err:?}",
    );
}

// §2.20 — FK column round-trips against author.
#[tokio::test]
async fn fk_column_round_trips() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "writer".into(),
        email: "w@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let author_id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let mut p = Post {
        id: Auto::Unset,
        title: "first".into(),
        slug: "first".into(),
        body: "hello".into(),
        author_id,
        published: false,
        view_count: 0,
        metadata: serde_json::json!({"tags": ["intro"]}),
        published_at: None,
    };
    p.save(&pool).await.unwrap();

    let posts: Vec<Post> = Post::objects()
        .filter("author_id", Op::Eq, author_id)
        .fetch(&pool).await.unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].title, "first");
    assert_eq!(posts[0].metadata["tags"][0], "intro");
}

// §2.26 — JSONB round-trips structured data.
#[tokio::test]
async fn jsonb_field_round_trips_structured_data() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "json".into(),
        email: "json@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let author_id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let payload = serde_json::json!({
        "tags": ["rust", "framework"],
        "stats": {"likes": 42, "shares": 7},
    });
    let mut p = Post {
        id: Auto::Unset,
        title: "j".into(),
        slug: "j".into(),
        body: "b".into(),
        author_id,
        published: true,
        view_count: 10,
        metadata: payload.clone(),
        published_at: Some(chrono::Utc::now()),
    };
    p.save(&pool).await.unwrap();

    let posts: Vec<Post> = Post::objects()
        .filter("slug", Op::Eq, "j")
        .fetch(&pool).await.unwrap();
    assert_eq!(posts[0].metadata, payload);
}

// §2.28 — TIMESTAMPTZ Option round-trips Some(...) and None.
#[tokio::test]
async fn datetime_option_round_trips() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "dt".into(),
        email: "dt@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let author_id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let when = chrono::Utc::now();
    let mut p1 = Post {
        id: Auto::Unset,
        title: "now".into(), slug: "now".into(),
        body: "b".into(),
        author_id,
        published: true,
        view_count: 0,
        metadata: serde_json::json!({}),
        published_at: Some(when),
    };
    p1.save(&pool).await.unwrap();

    let mut p2 = Post {
        id: Auto::Unset,
        title: "never".into(), slug: "never".into(),
        body: "b".into(),
        author_id,
        published: false,
        view_count: 0,
        metadata: serde_json::json!({}),
        published_at: None,
    };
    p2.save(&pool).await.unwrap();

    let now_back: Vec<Post> = Post::objects().filter("slug", Op::Eq, "now").fetch(&pool).await.unwrap();
    assert!(now_back[0].published_at.is_some(), "Some(when) lost on round-trip");
    let never_back: Vec<Post> = Post::objects().filter("slug", Op::Eq, "never").fetch(&pool).await.unwrap();
    assert_eq!(never_back[0].published_at, None);
}
