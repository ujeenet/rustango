//! Django-parity #352 — `ModelAdmin.list_select_related`. Tests the
//! per-model opt-out of the auto-JOIN policy and the explicit
//! whitelist form.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{ListSelectRelated, Model as _};
use rustango::sql::{ForeignKey, Pool};
use rustango::Model;
use tower::ServiceExt;

// Plain target table the parent FK points at.
#[derive(Model, Debug, Clone)]
#[rustango(table = "lsr_author", display = "name")]
#[allow(dead_code)]
pub struct LsrAuthor {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    name: String,
}

// Default — no `list_select_related` attr → ListSelectRelated::All.
#[derive(Model, Debug, Clone)]
#[rustango(table = "lsr_default_post", admin(list_display = "title,author_id"))]
#[allow(dead_code)]
pub struct LsrDefaultPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    author_id: ForeignKey<LsrAuthor, i64>,
}

// Opt-out — `list_select_related = "none"` skips the auto-JOIN.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "lsr_off_post",
    admin(list_display = "title,author_id", list_select_related = "none")
)]
#[allow(dead_code)]
pub struct LsrOffPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    author_id: ForeignKey<LsrAuthor, i64>,
}

// Whitelist — only join the named FK fields.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "lsr_only_post",
    admin(list_display = "title,author_id", list_select_related = "author_id")
)]
#[allow(dead_code)]
pub struct LsrOnlyPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    author_id: ForeignKey<LsrAuthor, i64>,
}

#[test]
fn default_resolves_to_all() {
    let cfg = LsrDefaultPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.list_select_related, ListSelectRelated::All);
}

#[test]
fn none_string_resolves_to_none_variant() {
    let cfg = LsrOffPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.list_select_related, ListSelectRelated::None);
}

#[test]
fn csv_string_resolves_to_only_variant() {
    let cfg = LsrOnlyPost::SCHEMA.admin.expect("admin attr set");
    match cfg.list_select_related {
        ListSelectRelated::Only(names) => assert_eq!(names, &["author_id"]),
        other => panic!("expected Only(..), got {other:?}"),
    }
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for ddl in [
        r#"CREATE TABLE IF NOT EXISTS "lsr_author" (
            "id"   INTEGER PRIMARY KEY AUTOINCREMENT,
            "name" TEXT NOT NULL
        )"#,
        r#"CREATE TABLE IF NOT EXISTS "lsr_default_post" (
            "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"     TEXT NOT NULL,
            "author_id" INTEGER NOT NULL REFERENCES "lsr_author"("id")
        )"#,
        r#"CREATE TABLE IF NOT EXISTS "lsr_off_post" (
            "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"     TEXT NOT NULL,
            "author_id" INTEGER NOT NULL REFERENCES "lsr_author"("id")
        )"#,
    ] {
        rustango::sql::raw_execute_pool(&pool, ddl, Vec::new())
            .await
            .expect("create");
    }
    // Seed one author + one post in each table; author's name should
    // appear in the JOIN-on cells but NOT when joins are off.
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "lsr_author" ("name") VALUES ('Asimov')"#,
        Vec::new(),
    )
    .await
    .expect("seed author");
    for table in ["lsr_default_post", "lsr_off_post"] {
        let sql = format!(r#"INSERT INTO "{table}" ("title", "author_id") VALUES ('Hello', 1)"#);
        rustango::sql::raw_execute_pool(&pool, &sql, Vec::new())
            .await
            .expect("seed");
    }
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn body_of(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn default_renders_joined_display_value() {
    let pool = build_pool().await;
    let body = body_of(pool, "/lsr_default_post").await;
    // Default joins → cell shows "Asimov" (the target's display field).
    assert!(
        body.contains("Asimov"),
        "default mode should render joined display value: {body}"
    );
}

#[tokio::test]
async fn none_renders_raw_pk() {
    let pool = build_pool().await;
    let body = body_of(pool, "/lsr_off_post").await;
    // Opt-out → no JOIN, the cell shows the raw author_id (1), not "Asimov".
    assert!(
        !body.contains("Asimov"),
        "list_select_related=none should NOT render joined display value: {body}"
    );
}
