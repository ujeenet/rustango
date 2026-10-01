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
use rustango::sql::{CounterPool as _, Pool};
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "sh_post",
    admin(actions = "delete_selected, restore_selected, touch_selected, ghost_selected")
)]
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
    let (pre_log, pre_pool) = (log.clone(), pool.clone());
    connect_admin_pre_delete(move |ctx: AdminDeleteContext| {
        let (log, pool) = (pre_log.clone(), pre_pool.clone());
        async move {
            // The row must still be there when pre_delete runs.
            let pk: i64 = ctx.pk.parse().unwrap();
            let n = ShPost::objects()
                .filter("id", pk)
                .count(&pool)
                .await
                .unwrap();
            log.lock().unwrap().push(format!("pre {} rows={n}", ctx.pk));
        }
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
    assert_eq!(
        *log.lock().unwrap(),
        ["pre 1 rows=1", "pre 2 rows=1", "post 1", "post 2"]
    );

    log.lock().unwrap().clear();
    post_form(&app, "/sh_post/3/delete", "").await;
    assert_eq!(*log.lock().unwrap(), ["pre 3 rows=1", "post 3"]);
    clear_all();
}

/// `(pre|post) <pk> change=<bool>` for every admin save signal.
fn log_saves(pool: &Pool) -> Arc<Mutex<Vec<String>>> {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (pre_log, pool) = (log.clone(), pool.clone());
    connect_admin_pre_save(move |ctx: AdminSaveContext| {
        let (log, pool) = (pre_log.clone(), pool.clone());
        async move {
            // The title the row had before the write.
            let rows: Vec<ShPost> = rustango::sql::FetcherPool::fetch(ShPost::objects(), &pool)
                .await
                .unwrap();
            let title = rows.first().map_or("-".to_owned(), |r| r.title.clone());
            log.lock()
                .unwrap()
                .push(format!("pre {} {} {title}", ctx.pk, ctx.change));
        }
    });
    let post_log = log.clone();
    connect_admin_post_save(move |ctx: AdminSaveContext| {
        let log = post_log.clone();
        async move {
            log.lock()
                .unwrap()
                .push(format!("post {} {}", ctx.pk, ctx.change))
        }
    });
    log
}

fn noop<'a>(
    _: &'a Pool,
    _: &'a [rustango::core::SqlValue],
) -> rustango::admin::AdminActionFuture<'a> {
    Box::pin(async { Ok(()) })
}

/// An edit sends pre_save before the UPDATE, then post_save.
#[tokio::test]
async fn update_sends_pre_save_before_the_write() {
    let _guard = signal_lock().lock().await;
    clear_all();
    let pool = build_pool().await;
    let app = build_app(pool.clone());
    post_form(&app, "/sh_post", "title=Seeded").await;
    let log = log_saves(&pool);

    let status = post_form(&app, "/sh_post/1", "title=Renamed").await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(*log.lock().unwrap(), ["pre 1 true Seeded", "post 1 true"]);
    clear_all();
}

/// A custom bulk action sends a save pair (`change = true`) for every row.
#[tokio::test]
async fn custom_action_sends_per_row_save_signals() {
    let _guard = signal_lock().lock().await;
    clear_all();
    let pool = build_pool().await;
    let app = build_app(pool.clone());
    for t in ["a", "b"] {
        post_form(&app, "/sh_post", &format!("title={t}")).await;
    }
    let log = log_saves(&pool);
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .register_action("sh_post", "touch_selected", noop)
        .build();
    let status = post_form(
        &app,
        "/sh_post/__action",
        "action=touch_selected&_selected=1&_selected=2",
    )
    .await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(
        *log.lock().unwrap(),
        ["pre 1 true a", "pre 2 true a", "post 1 true", "post 2 true"]
    );
    clear_all();
}

/// A bulk action that is refused, or does nothing, sends no signal at all.
#[tokio::test]
async fn refused_or_noop_bulk_action_sends_no_signal() {
    let _guard = signal_lock().lock().await;
    clear_all();
    let pool = build_pool().await;
    post_form(&build_app(pool.clone()), "/sh_post", "title=a").await;
    let log = log_saves(&pool);
    let del_log = log.clone();
    connect_admin_pre_delete(move |ctx: AdminDeleteContext| {
        let log = del_log.clone();
        async move { log.lock().unwrap().push(format!("pre-delete {}", ctx.pk)) }
    });

    let no_delete = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_user_perms(["sh_post.view".to_owned(), "sh_post.change".to_owned()])
        .build();
    let read_only = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .read_only(["sh_post"])
        .register_action("sh_post", "touch_selected", noop)
        .build();
    let plain = build_app(pool.clone());
    let mut signalled = Vec::new();
    for (app, action, want) in [
        (&no_delete, "delete_selected", StatusCode::FORBIDDEN),
        (&read_only, "touch_selected", StatusCode::FORBIDDEN),
        (&plain, "ghost_selected", StatusCode::INTERNAL_SERVER_ERROR),
        // `sh_post` has no soft-delete column, so restore is a no-op.
        (&plain, "restore_selected", StatusCode::SEE_OTHER),
    ] {
        let form = format!("action={action}&_selected=1");
        let status = post_form(app, "/sh_post/__action", &form).await;
        assert_eq!(status, want, "{action}");
        if !std::mem::take(&mut *log.lock().unwrap()).is_empty() {
            signalled.push(action);
        }
    }
    assert!(signalled.is_empty(), "signals sent by {signalled:?}");
    let count = ShPost::objects().count(&pool).await.unwrap();
    assert_eq!(count, 1, "nothing deleted");
    clear_all();
}
