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
        "DROP TABLE IF EXISTS cookbook_post_tag CASCADE",
        "DROP TABLE IF EXISTS cookbook_tag CASCADE",
        "DROP TABLE IF EXISTS cookbook_post CASCADE",
        "DROP TABLE IF EXISTS cookbook_author_profile CASCADE",
        "DROP TABLE IF EXISTS cookbook_rating CASCADE",
        "DROP TABLE IF EXISTS cookbook_author CASCADE",
        "DROP TABLE IF EXISTS cookbook_session CASCADE",
        "DROP TABLE IF EXISTS cookbook_archive_note CASCADE",
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

    // §2.27 Auto<Uuid>
    sqlx::query(r#"CREATE EXTENSION IF NOT EXISTS pgcrypto"#)
        .execute(pool).await.expect("pgcrypto");
    sqlx::query(
        r#"CREATE TABLE cookbook_session (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            user_token VARCHAR(80) NOT NULL
        )"#,
    ).execute(pool).await.expect("create session");

    // §2.21 O2O — UNIQUE on author_id makes it 1:1
    sqlx::query(
        r#"CREATE TABLE cookbook_author_profile (
            id BIGSERIAL PRIMARY KEY,
            author_id BIGINT NOT NULL UNIQUE REFERENCES cookbook_author(id),
            avatar_url TEXT NOT NULL
        )"#,
    ).execute(pool).await.expect("create profile");

    // §2.22 M2M
    sqlx::query(
        r#"CREATE TABLE cookbook_tag (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(40) NOT NULL UNIQUE
        )"#,
    ).execute(pool).await.expect("create tag");
    sqlx::query(
        r#"CREATE TABLE cookbook_post_tag (
            post_id BIGINT NOT NULL REFERENCES cookbook_post(id),
            tag_id  BIGINT NOT NULL REFERENCES cookbook_tag(id),
            PRIMARY KEY (post_id, tag_id)
        )"#,
    ).execute(pool).await.expect("create post_tag");

    // §2.30 soft_delete
    sqlx::query(
        r#"CREATE TABLE cookbook_archive_note (
            id BIGSERIAL PRIMARY KEY,
            note VARCHAR(200) NOT NULL,
            deleted_at TIMESTAMPTZ NULL
        )"#,
    ).execute(pool).await.expect("create archive_note");
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

// §2.27 — Auto<Uuid> + auto_uuid mixin assigns a v4 UUID server-side.
#[tokio::test]
async fn auto_uuid_assigns_server_side_uuid() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut s = Session { id: Auto::Unset, user_token: "tok".into() };
    s.save(&pool).await.expect("save session");
    let id = match s.id { Auto::Set(v) => v, Auto::Unset => panic!("Auto<Uuid> not assigned") };
    assert_ne!(id, uuid::Uuid::nil(), "DB DEFAULT gen_random_uuid() should fill a real v4");
}

// §2.21 — O2O UNIQUE FK rejects a second row with the same author_id.
#[tokio::test]
async fn o2o_unique_fk_rejects_duplicate() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "p".into(), email: "p@example.com".into(),
        bio: None, joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let author_id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let mut p1 = AuthorProfile { id: Auto::Unset, author_id, avatar_url: "/a.png".into() };
    p1.save(&pool).await.expect("first profile");
    let mut p2 = AuthorProfile { id: Auto::Unset, author_id, avatar_url: "/b.png".into() };
    let err = p2.save(&pool).await.expect_err("o2o duplicate must fail");
    let msg = format!("{err:?}").to_lowercase();
    assert!(msg.contains("unique") || msg.contains("duplicate"),
        "expected unique violation, got {err:?}");
}

// §2.22 — M2M through writes/reads the junction table.
#[tokio::test]
async fn m2m_through_junction_table_round_trips() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "m2m".into(), email: "m@example.com".into(),
        bio: None, joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let author_id = match a.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let mut p = Post {
        id: Auto::Unset,
        title: "tagged".into(), slug: "tagged".into(),
        body: "b".into(), author_id,
        published: false, view_count: 0,
        metadata: serde_json::json!({}),
        published_at: None,
    };
    p.save(&pool).await.unwrap();
    let post_id = match p.id { Auto::Set(v) => v, Auto::Unset => unreachable!() };

    let mut t1 = Tag { id: Auto::Unset, name: "rust".into() };
    t1.save(&pool).await.unwrap();
    let mut t2 = Tag { id: Auto::Unset, name: "framework".into() };
    t2.save(&pool).await.unwrap();
    let t1_id = match t1.id { Auto::Set(v) => v, _ => unreachable!() };
    let t2_id = match t2.id { Auto::Set(v) => v, _ => unreachable!() };

    sqlx::query("INSERT INTO cookbook_post_tag (post_id, tag_id) VALUES ($1, $2), ($1, $3)")
        .bind(post_id).bind(t1_id).bind(t2_id)
        .execute(&pool).await.expect("link tags");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cookbook_post_tag WHERE post_id = $1"
    ).bind(post_id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2, "junction should hold 2 rows for one post");
}

// §2.30 — soft_delete column survives + deleted_at column writes a NULL by default.
#[tokio::test]
async fn soft_delete_column_round_trips_and_deleted_at_defaults_null() {
    let Some(pool) = pool().await else { return };
    fresh_schema(&pool).await;

    let mut n = ArchiveNote {
        id: Auto::Unset,
        note: "alive".into(),
        deleted_at: None,
    };
    n.save(&pool).await.unwrap();
    let id = match n.id { Auto::Set(v) => v, _ => unreachable!() };

    let rows: Vec<ArchiveNote> = ArchiveNote::objects()
        .filter("id", Op::Eq, id)
        .fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].deleted_at, None, "fresh row deleted_at should be NULL");
}
