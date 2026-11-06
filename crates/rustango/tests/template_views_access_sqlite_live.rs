//! Template views mount only with an access decision, and enforce it: an
//! anonymous POST to a gated DeleteView deletes nothing.

#![cfg(all(feature = "sqlite", feature = "template_views"))]
#![allow(irrefutable_let_patterns)] // `Pool` is single-variant in a sqlite-only build.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::template_views::{CreateView, DeleteView, DetailView, ListView, UpdateView};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "tv_access_post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

const CSRF: &str = "tv-access-csrf-token-tv-access-csrf-token-x";

async fn pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE tv_access_post (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .unwrap();
    sqlx::query("INSERT INTO tv_access_post (id, title) VALUES (7, 'seven')")
        .execute(&sq)
        .await
        .unwrap();
    Pool::Sqlite(sq)
}

fn tera() -> Arc<tera::Tera> {
    let mut t = tera::Tera::default();
    for name in [
        "tv_access_post_list.html",
        "tv_access_post_detail.html",
        "tv_access_post_form.html",
        "tv_access_post_confirm_delete.html",
    ] {
        t.add_raw_template(name, "ok").unwrap();
    }
    Arc::new(t)
}

/// The panic message of a mount, or `None` when it mounted.
fn refusal(mount: impl FnOnce() -> axum::Router) -> Option<String> {
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = mount();
    }))
    .err()?;
    Some(
        err.downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| err.downcast_ref::<&str>().unwrap_or(&"").to_string()),
    )
}

async fn rows(pool: &Pool) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query_scalar("SELECT COUNT(*) FROM tv_access_post")
        .fetch_one(sq)
        .await
        .unwrap()
}

fn delete_post() -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/posts/7/delete")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
        .body(Body::from(format!("_csrf={CSRF}")))
        .unwrap()
}

/// Every view, read or write, refuses to mount until access is decided,
/// and the message names a fix this build has.
#[tokio::test]
async fn every_view_refuses_to_mount_undecided() {
    let (p, t) = (pool().await, tera());
    let mounts: Vec<(&str, Box<dyn FnOnce() -> axum::Router>)> = vec![
        ("ListView", {
            let (p, t) = (p.clone(), t.clone());
            Box::new(move || ListView::for_model(Post::SCHEMA).router("/posts", t, p))
        }),
        ("DetailView", {
            let (p, t) = (p.clone(), t.clone());
            Box::new(move || DetailView::for_model(Post::SCHEMA).router("/posts", t, p))
        }),
        ("CreateView", {
            let (p, t) = (p.clone(), t.clone());
            Box::new(move || CreateView::for_model(Post::SCHEMA, &["title"]).router("/posts", t, p))
        }),
        ("UpdateView", {
            let (p, t) = (p.clone(), t.clone());
            Box::new(move || UpdateView::for_model(Post::SCHEMA, &["title"]).router("/posts", t, p))
        }),
        ("DeleteView", {
            let (p, t) = (p.clone(), t.clone());
            Box::new(move || DeleteView::for_model(Post::SCHEMA).router("/posts", t, p))
        }),
    ];
    for (view, mount) in mounts {
        let msg = refusal(mount).unwrap_or_else(|| panic!("{view} mounted with no decision"));
        assert!(
            msg.contains(view) && msg.contains("tv_access_post"),
            "{msg}"
        );
        assert!(msg.contains("allow_anonymous"), "{msg}");
        #[cfg(feature = "tenancy")]
        assert!(msg.contains("permissions_for_model"), "{msg}");
        #[cfg(not(feature = "tenancy"))]
        assert!(!msg.contains("permissions"), "{msg}");
    }
    // Bulk actions make a ListView a write surface; the decision covers it.
    assert!(refusal(
        || ListView::for_model(Post::SCHEMA).bulk_actions(true).router(
            "/posts",
            t.clone(),
            p.clone()
        )
    )
    .is_some());
}

/// `allow_anonymous` is the explicit open door: it mounts and serves.
#[tokio::test]
async fn allow_anonymous_serves_anyone() {
    let p = pool().await;
    let app = DeleteView::for_model(Post::SCHEMA)
        .allow_anonymous()
        .router("/posts", tera(), p.clone());
    let res = app.oneshot(delete_post()).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(rows(&p).await, 0);
}

#[cfg(feature = "tenancy")]
mod gated {
    use super::*;
    use rustango::tenancy::middleware::AuthenticatedUser;

    async fn framework_pool() -> Pool {
        let p = pool().await;
        rustango::testkit::create_tables_for::<rustango::tenancy::User>(&p)
            .await
            .unwrap();
        rustango::testkit::migrate_framework(&p).await.unwrap();
        let Pool::Sqlite(sq) = &p else { unreachable!() };
        sqlx::query(
            "INSERT INTO rustango_users (id, username, password_hash, is_superuser, active, \
             created_at) VALUES (5, 'ada', '', 0, 1, datetime('now'))",
        )
        .execute(sq)
        .await
        .unwrap();
        p
    }

    fn as_user(mut req: Request<Body>, id: i64) -> Request<Body> {
        req.extensions_mut().insert(AuthenticatedUser {
            id,
            username: "ada".into(),
            is_superuser: false,
        });
        req
    }

    /// The reported attack: an anonymous POST to a gated DeleteView is a
    /// 401 and the row stays; a user without the codename gets 403.
    #[tokio::test]
    async fn delete_view_needs_the_delete_codename() {
        let p = framework_pool().await;
        let app = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .router("/posts", tera(), p.clone());

        let res = app.clone().oneshot(delete_post()).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app
            .clone()
            .oneshot(as_user(delete_post(), 5))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(rows(&p).await, 1, "no denied request may delete");

        rustango::tenancy::permissions::set_user_perm_pool(5, "tv_access_post.delete", true, &p)
            .await
            .unwrap();
        let res = app.oneshot(as_user(delete_post(), 5)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(rows(&p).await, 0);
    }

    /// `login_required` lets any signed-in user through and nobody else.
    #[tokio::test]
    async fn login_required_needs_a_user() {
        let p = framework_pool().await;
        let app = DetailView::for_model(Post::SCHEMA)
            .login_required()
            .router("/posts", tera(), p);
        let get = || {
            Request::builder()
                .uri("/posts/7")
                .body(Body::empty())
                .unwrap()
        };
        let res = app.clone().oneshot(get()).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app.oneshot(as_user(get(), 5)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A ListView's bulk POST needs more than its GET.
    #[tokio::test]
    async fn bulk_actions_need_the_write_codenames() {
        let p = framework_pool().await;
        rustango::tenancy::permissions::set_user_perm_pool(5, "tv_access_post.view", true, &p)
            .await
            .unwrap();
        let app = ListView::for_model(Post::SCHEMA)
            .bulk_actions(true)
            .permissions_for_model()
            .router("/posts", tera(), p.clone());
        let res = app
            .clone()
            .oneshot(as_user(
                Request::builder()
                    .uri("/posts")
                    .body(Body::empty())
                    .unwrap(),
                5,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bulk = Request::builder()
            .method(Method::POST)
            .uri("/posts")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
            .body(Body::from(format!(
                "_csrf={CSRF}&action=delete_selected&_selected_action=7"
            )))
            .unwrap();
        let res = app.oneshot(as_user(bulk, 5)).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(rows(&p).await, 1);
    }
}
