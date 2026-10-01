//! Issue #365 — admin `save_model` / `delete_model` hooks.
//!
//! Verifies that admin signals fire on create / update / delete with
//! the right context, pre hooks included, in registration order and per
//! bulk-action row (#1928).

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::signals::admin::{
    clear_all, connect_admin_post_delete, connect_admin_post_save, connect_admin_pre_delete,
    connect_admin_pre_save, AdminDeleteContext, AdminSaveContext,
};
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sh_post", admin(actions = "delete_selected"))]
#[allow(dead_code)]
pub struct ShPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

/// Per-suite mutex — admin signals share a global registry, so tests
/// that connect receivers must run serialized to keep
/// per-test assertions clean.
fn signal_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "sh_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

#[tokio::test]
async fn post_save_fires_on_create() {
    let _guard = signal_lock().lock().await;
    clear_all();

    let saves: Arc<Mutex<Vec<AdminSaveContext>>> = Arc::new(Mutex::new(Vec::new()));
    let saves_for_handler = saves.clone();
    connect_admin_post_save(move |ctx| {
        let saves = saves_for_handler.clone();
        async move {
            saves.lock().unwrap().push(ctx);
        }
    });

    let pool = build_pool().await;
    let app = build_app(pool.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sh_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=Hello"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "create POST failed: {}",
        resp.status()
    );

    let captured = saves.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        1,
        "expected 1 post-save event: {captured:?}"
    );
    assert_eq!(captured[0].table, "sh_post");
    assert!(!captured[0].change, "create event must report change=false");
    assert!(
        !captured[0].pk.is_empty(),
        "pk must be populated after create"
    );

    clear_all();
}

#[tokio::test]
async fn post_save_fires_on_update() {
    let _guard = signal_lock().lock().await;
    clear_all();

    let pool = build_pool().await;
    let app = build_app(pool.clone());
    // Seed a row.
    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sh_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=Seeded"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(create.status() == StatusCode::SEE_OTHER || create.status() == StatusCode::OK);

    // Now subscribe and update.
    let saves: Arc<Mutex<Vec<AdminSaveContext>>> = Arc::new(Mutex::new(Vec::new()));
    let saves_for_handler = saves.clone();
    connect_admin_post_save(move |ctx| {
        let saves = saves_for_handler.clone();
        async move {
            saves.lock().unwrap().push(ctx);
        }
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sh_post/1")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=Renamed"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK);

    let captured = saves.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "expected 1 post-save: {captured:?}");
    assert_eq!(captured[0].table, "sh_post");
    assert_eq!(captured[0].pk, "1");
    assert!(captured[0].change, "update event must report change=true");

    clear_all();
}

#[tokio::test]
async fn post_delete_fires_on_delete() {
    let _guard = signal_lock().lock().await;
    clear_all();

    let pool = build_pool().await;
    let app = build_app(pool.clone());
    // Seed.
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sh_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=ToDelete"))
                .unwrap(),
        )
        .await
        .unwrap();

    let deletes: Arc<Mutex<Vec<AdminDeleteContext>>> = Arc::new(Mutex::new(Vec::new()));
    let deletes_for_handler = deletes.clone();
    connect_admin_post_delete(move |ctx| {
        let deletes = deletes_for_handler.clone();
        async move {
            deletes.lock().unwrap().push(ctx);
        }
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sh_post/1/delete")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK);

    let captured = deletes.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "expected 1 post-delete: {captured:?}");
    assert_eq!(captured[0].table, "sh_post");
    assert_eq!(captured[0].pk, "1");

    clear_all();
}

async fn post_form(app: &axum::Router, uri: &str, body: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Pre hooks run before the write, and receivers run in registration order.
#[tokio::test]
async fn pre_save_runs_first_and_receivers_keep_their_order() {
    let _guard = signal_lock().lock().await;
    clear_all();
    let pool = build_pool().await;
    let app = build_app(pool.clone());

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    for n in 0..16 {
        let (log, pool) = (log.clone(), pool.clone());
        connect_admin_pre_save(move |_ctx| {
            let (log, pool) = (log.clone(), pool.clone());
            async move {
                let rows: Vec<ShPost> = rustango::sql::FetcherPool::fetch(ShPost::objects(), &pool)
                    .await
                    .unwrap();
                log.lock().unwrap().push(format!("pre{n}:{}", rows.len()));
            }
        });
    }
    let post_log = log.clone();
    connect_admin_post_save(move |_ctx| {
        let log = post_log.clone();
        async move { log.lock().unwrap().push("post".into()) }
    });

    let status = post_form(&app, "/sh_post", "title=First").await;
    assert!(
        status.is_redirection() || status == StatusCode::OK,
        "{status}"
    );
    let mut want: Vec<String> = (0..16).map(|n| format!("pre{n}:0")).collect();
    want.push("post".into());
    assert_eq!(*log.lock().unwrap(), want);
    clear_all();
}

/// `delete_selected` sends a pre and post delete for every row it removes.
#[tokio::test]
async fn delete_selected_sends_per_row_delete_signals() {
    let _guard = signal_lock().lock().await;
    clear_all();
    let pool = build_pool().await;
    let app = build_app(pool.clone());
    for t in ["a", "b", "c"] {
        post_form(&app, "/sh_post", &format!("title={t}")).await;
    }

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let pre_log = log.clone();
    connect_admin_pre_delete(move |ctx: AdminDeleteContext| {
        let log = pre_log.clone();
        async move { log.lock().unwrap().push(format!("pre {}", ctx.pk)) }
    });
    let post_log = log.clone();
    connect_admin_post_delete(move |ctx: AdminDeleteContext| {
        let log = post_log.clone();
        async move { log.lock().unwrap().push(format!("post {}", ctx.pk)) }
    });

    let status = post_form(
        &app,
        "/sh_post/__action",
        "action=delete_selected&_selected=1&_selected=2",
    )
    .await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(*log.lock().unwrap(), ["pre 1", "pre 2", "post 1", "post 2"]);

    log.lock().unwrap().clear();
    post_form(&app, "/sh_post/3/delete", "").await;
    assert_eq!(*log.lock().unwrap(), ["pre 3", "post 3"]);
    clear_all();
}
