//! Live regression for the v0.34 bi-dialect `contenttypes::*_pool`
//! family. Exercises the new `&Pool`-taking helpers against an
//! in-memory SQLite registry — proves a sqlite-only stack can
//! bootstrap + seed `rustango_content_types` without ever touching
//! Postgres.
//!
//! Companion to `contenttypes_live.rs` (which is PG-gated on
//! `DATABASE_URL`). This file is unconditional — sqlite-in-memory has
//! no infra requirements.

#![cfg(feature = "sqlite")]

use rustango::contenttypes::{self, ContentType};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

/// Unique-table dummy model — sits in the process-global inventory so
/// `ensure_seeded_pool` has at least one row to insert beyond the
/// ContentType row itself (which is excluded).
#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_pool_live_post")]
#[rustango(app = "blog_pool_live")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_pool_live_user")]
#[rustango(app = "auth_pool_live")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub username: String,
}

async fn sqlite_pool() -> Pool {
    Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool"),
    )
}

#[tokio::test]
async fn ensure_table_pool_creates_sqlite_table() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_table_pool(&pool)
        .await
        .expect("ensure_table_pool");
    // Idempotent — second call is a no-op.
    contenttypes::ensure_table_pool(&pool)
        .await
        .expect("ensure_table_pool idempotent");

    // Probe that the table actually exists by issuing a SELECT.
    if let Pool::Sqlite(sq) = &pool {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rustango_content_types")
            .fetch_one(sq)
            .await
            .expect("select count");
        assert_eq!(count, 0, "table should exist and be empty");
    } else {
        panic!("expected sqlite pool");
    }
}

#[tokio::test]
async fn ensure_seeded_pool_inserts_rows_on_sqlite() {
    let pool = sqlite_pool().await;
    let inserted = contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("ensure_seeded_pool");
    // Inventory is process-global: every Model from every test in
    // this binary is registered. The minimum guarantee is that the
    // two dummy models above made it in.
    assert!(
        inserted >= 2,
        "expected at least 2 inserted (Post + User from this test), got {inserted}"
    );
}

#[tokio::test]
async fn ensure_seeded_pool_is_idempotent_on_sqlite() {
    let pool = sqlite_pool().await;
    let first = contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("first seed");
    assert!(first >= 1);
    let second = contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("second seed");
    assert_eq!(second, 0, "re-seed should insert nothing");
}

#[tokio::test]
async fn by_natural_key_pool_finds_seeded_row_on_sqlite() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let row = ContentType::by_natural_key_pool(&pool, "blog_pool_live", "post")
        .await
        .expect("lookup");
    let row = row.expect("seeded row should exist");
    assert_eq!(row.app_label, "blog_pool_live");
    assert_eq!(row.model_name, "post");
    assert_eq!(row.table, "ct_pool_live_post");
}

#[tokio::test]
async fn by_natural_key_pool_returns_none_for_unknown_key() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let row = ContentType::by_natural_key_pool(&pool, "nope", "missing")
        .await
        .expect("lookup");
    assert!(row.is_none());
}

// ============================================================ slice 25/26c
// New `_pool` companions added during the v0.38 audit pass — these need
// runtime coverage on sqlite (the PG path is exercised via the back-compat
// shims in `contenttypes_live.rs`).

#[tokio::test]
async fn for_model_pool_resolves_model_type_on_sqlite() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let row = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("for_model_pool");
    let row = row.expect("Post should have a CT row");
    assert_eq!(row.app_label, "blog_pool_live");
    assert_eq!(row.model_name, "post");
    assert_eq!(row.table, "ct_pool_live_post");
}

#[tokio::test]
async fn for_model_pool_finds_user_too() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let row = ContentType::for_model_pool::<User>(&pool)
        .await
        .expect("for_model_pool")
        .expect("User should have a CT row");
    assert_eq!(row.app_label, "auth_pool_live");
    assert_eq!(row.model_name, "user");
}

#[tokio::test]
async fn all_pool_returns_seeded_rows_alphabetically() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let rows = ContentType::all_pool(&pool).await.expect("all_pool");
    assert!(
        rows.len() >= 2,
        "should have at least the Post + User rows; got {}",
        rows.len()
    );
    // Spot-check: rows ordered by (app_label, model_name) — `auth_pool_live`
    // < `blog_pool_live` alphabetically.
    let mut auth_idx = None;
    let mut blog_idx = None;
    for (i, r) in rows.iter().enumerate() {
        if r.app_label == "auth_pool_live" {
            auth_idx = Some(i);
        }
        if r.app_label == "blog_pool_live" {
            blog_idx = Some(i);
        }
    }
    let (a, b) = (auth_idx.expect("auth row"), blog_idx.expect("blog row"));
    assert!(a < b, "auth_pool_live should sort before blog_pool_live");
}

#[tokio::test]
async fn for_target_pool_constructs_generic_fk_on_sqlite() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let gfk = rustango::contenttypes::GenericForeignKey::for_target_pool::<Post>(&pool, 42)
        .await
        .expect("for_target_pool");
    assert_eq!(gfk.object_pk, 42);
    // ct id should match what for_model_pool returns.
    let ct = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("for_model_pool")
        .expect("ct row");
    assert_eq!(
        Some(gfk.content_type_id),
        ct.id.get().copied(),
        "GFK content_type_id should equal Post's CT row id"
    );
}

#[tokio::test]
async fn fetch_row_as_json_pool_returns_none_for_missing_pk() {
    use rustango::sql::sqlx::Executor as _;
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    // Bootstrap the Post table so `fetch_row_as_json_pool` finds the
    // schema and can issue the SELECT — and returns None for an
    // absent PK.
    if let Pool::Sqlite(sq) = &pool {
        sq.execute(
            "CREATE TABLE IF NOT EXISTS ct_pool_live_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .await
        .expect("create post table");
    }
    let ct = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("for_model_pool")
        .expect("ct row");
    let row = contenttypes::fetch_row_as_json_pool(&pool, &ct, 9999_i64)
        .await
        .expect("fetch_row_as_json_pool");
    assert!(row.is_none(), "no row with id=9999 should return None");
}

#[tokio::test]
async fn render_generic_fk_link_pool_emits_clickable_html_on_sqlite() {
    // Coverage for `render_generic_fk_link_pool` — slice 26c. Used by
    // the admin to render `(content_type_id, pk)` as a link. Returns
    // graceful fallback HTML when the CT row is unknown.
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let ct = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("for_model_pool")
        .expect("ct row");
    let ct_id = ct.id.get().copied().expect("ct id");
    let gfk = rustango::contenttypes::GenericForeignKey::new(ct_id, 7);
    let html = contenttypes::render_generic_fk_link_pool(&pool, gfk)
        .await
        .expect("render_generic_fk_link_pool");
    assert!(
        html.contains("ct_pool_live_post"),
        "rendered link should mention the target table, got: {html}"
    );
    assert!(html.contains(">"), "should be HTML, got: {html}");
    assert!(html.contains("#7"), "should mention the pk, got: {html}");

    // Unknown CT id → graceful fallback HTML, not an error.
    let fallback = contenttypes::render_generic_fk_link_pool(
        &pool,
        rustango::contenttypes::GenericForeignKey::new(99_999, 7),
    )
    .await
    .expect("fallback ok");
    assert!(
        fallback.contains("ct=99999"),
        "unknown CT should render the raw (ct, pk) tuple, got: {fallback}"
    );
}

/// Model with an explicit `author_id: i64` soft-FK column for the
/// prefetch tests. Separate from `Post` above so the schema field
/// resolves at compile time.
#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_pool_live_comment")]
#[rustango(app = "blog_pool_live")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub body: String,
    /// Soft FK — column lives on the row, no `rustango(fk = …)` so
    /// the framework treats it as a plain i64.
    pub author_id: i64,
}

#[tokio::test]
async fn prefetch_soft_pool_groups_children_by_parent_pk_on_sqlite() {
    // Coverage for `prefetch_soft_pool` — slice 26c. Takes a list of
    // parent PKs + a soft-FK column name, runs one SELECT against the
    // child table filtering `WHERE <fk_col> IN (...)`, returns a
    // HashMap<parent_pk, Vec<Child>>.
    use rustango::sql::sqlx::Executor as _;
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    if let Pool::Sqlite(sq) = &pool {
        sq.execute(
            "CREATE TABLE IF NOT EXISTS ct_pool_live_comment (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                body TEXT NOT NULL, \
                author_id INTEGER NOT NULL)",
        )
        .await
        .expect("create");
        for (body, author) in [("a", 1_i64), ("b", 1_i64), ("c", 2_i64)] {
            sqlx::query("INSERT INTO ct_pool_live_comment (body, author_id) VALUES (?, ?)")
                .bind(body)
                .bind(author)
                .execute(sq)
                .await
                .expect("insert");
        }
    }
    let grouped =
        contenttypes::prefetch_soft_pool::<Comment, _>(&pool, &[1, 2], "author_id", |c| {
            c.author_id
        })
        .await
        .expect("prefetch_soft_pool");
    assert_eq!(
        grouped.get(&1).map_or(0, |v| v.len()),
        2,
        "author 1 has 2 comments"
    );
    assert_eq!(
        grouped.get(&2).map_or(0, |v| v.len()),
        1,
        "author 2 has 1 comment"
    );

    // Empty parent_pks list short-circuits to empty map.
    let empty =
        contenttypes::prefetch_soft_pool::<Comment, _>(&pool, &[], "author_id", |c| c.author_id)
            .await
            .expect("empty pks");
    assert!(empty.is_empty(), "empty parent list short-circuits");
}

#[tokio::test]
async fn prefetch_generic_pool_hydrates_targets_on_sqlite() {
    // Coverage for `prefetch_generic_pool` — slice 26c. Resolves the
    // ContentType for `C`, runs one SELECT for every (ct_id, pk) pair
    // whose ct matches, returns `HashMap<(i64, i64), C>`.
    use rustango::sql::sqlx::Executor as _;
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    if let Pool::Sqlite(sq) = &pool {
        sq.execute(
            "CREATE TABLE IF NOT EXISTS ct_pool_live_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .await
        .expect("create");
        sqlx::query("INSERT INTO ct_pool_live_post (title) VALUES ('hello')")
            .execute(sq)
            .await
            .expect("seed row");
    }
    // Look up Post's CT id then ask prefetch_generic_pool to hydrate.
    let ct = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("for_model_pool")
        .expect("ct row");
    let ct_id = ct.id.get().copied().expect("ct id");
    let map = contenttypes::prefetch_generic_pool::<Post>(&pool, &[(ct_id, 1)])
        .await
        .expect("prefetch_generic_pool");
    assert!(
        map.contains_key(&(ct_id, 1)),
        "expected (ct_id, 1) in map, got keys: {:?}",
        map.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        map.get(&(ct_id, 1)).map(|p| p.title.as_str()),
        Some("hello"),
        "hydrated Post should carry the seeded title"
    );

    // Empty pairs short-circuit to empty map.
    let empty = contenttypes::prefetch_generic_pool::<Post>(&pool, &[])
        .await
        .expect("empty");
    assert!(empty.is_empty(), "empty pair list should short-circuit");
}

#[tokio::test]
async fn for_each_row_of_ct_pool_visits_seeded_rows() {
    use rustango::sql::sqlx::Executor as _;
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    // Bootstrap + seed three Posts so the iterator has something to walk.
    if let Pool::Sqlite(sq) = &pool {
        sq.execute(
            "CREATE TABLE IF NOT EXISTS ct_pool_live_post (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                title TEXT NOT NULL)",
        )
        .await
        .expect("create post table");
        for title in ["alpha", "beta", "gamma"] {
            sqlx::query("INSERT INTO ct_pool_live_post (title) VALUES (?)")
                .bind(title)
                .execute(sq)
                .await
                .expect("insert");
        }
    }
    let ct = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("for_model_pool")
        .expect("ct row");
    let mut visited = 0usize;
    let total = contenttypes::for_each_row_of_ct_pool(&pool, &ct, 2, |_row| {
        visited += 1;
        Ok(())
    })
    .await
    .expect("for_each_row_of_ct_pool");
    assert_eq!(total, 3, "should visit all three Post rows");
    assert_eq!(visited, 3, "closure should fire once per row");
}
