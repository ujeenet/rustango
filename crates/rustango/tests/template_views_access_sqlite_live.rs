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

    fn as_user(req: Request<Body>, id: i64) -> Request<Body> {
        signed_in(req, id, false)
    }

    pub(super) fn signed_in(mut req: Request<Body>, id: i64, is_superuser: bool) -> Request<Body> {
        req.extensions_mut().insert(AuthenticatedUser {
            id,
            username: "ada".into(),
            is_superuser,
        });
        req
    }

    /// As `require_auth` leaves it: the user, bound to tenant `acme` (id 1).
    pub(super) fn of_acme(req: Request<Body>, id: i64, is_superuser: bool) -> Request<Body> {
        let mut req = signed_in(req, id, is_superuser);
        req.extensions_mut()
            .insert(rustango::tenancy::TenantSlug::new(1, "acme"));
        req
    }

    async fn grant(p: &Pool, actions: &[&str]) {
        for a in actions {
            let cn = format!("tv_access_post.{a}");
            rustango::tenancy::permissions::set_user_perm_pool(5, &cn, true, p)
                .await
                .unwrap();
        }
    }

    /// A fixed pool belongs to no tenant: a tenant's user id or superuser
    /// bit means nothing in it, even with the codename granted here.
    #[tokio::test]
    async fn a_tenant_credential_is_refused_on_a_fixed_pool() {
        let p = framework_pool().await;
        grant(&p, &["delete"]).await;
        let app = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .router("/posts", tera(), p.clone());
        for superuser in [false, true] {
            let res = app
                .clone()
                .oneshot(of_acme(delete_post(), 5, superuser))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "superuser={superuser}");
        }
        assert_eq!(rows(&p).await, 1);
    }

    /// Routed to a tenant, with no `TenantSlug` on the credential: still a
    /// tenant's user, so refused on a fixed pool.
    #[tokio::test]
    async fn a_tenant_routed_request_is_refused_on_a_fixed_pool() {
        let p = framework_pool().await;
        let app = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .router("/posts", tera(), p.clone());
        let mut req = signed_in(delete_post(), 5, true);
        req.extensions_mut()
            .insert(rustango::tenancy::RequestTenant::Tenant(
                rustango::tenancy::TenantSlug::new(1, "acme"),
            ));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(rows(&p).await, 1);
    }

    /// An untenanted superuser passes every codename on a fixed pool.
    #[tokio::test]
    async fn a_superuser_passes() {
        let p = framework_pool().await;
        let app = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .router("/posts", tera(), p.clone());
        let res = app
            .oneshot(signed_in(delete_post(), 5, true))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(rows(&p).await, 0);
    }

    /// A permission read that fails is an outage (503), not a denial.
    #[tokio::test]
    async fn a_failed_permission_read_is_503() {
        let p = pool().await; // no permission tables
        let app = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .router("/posts", tera(), p.clone());
        let res = app.oneshot(as_user(delete_post(), 5)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rows(&p).await, 1);
    }

    /// `.permissions(&[])` would mean "any signed-in user": refused at mount.
    #[tokio::test]
    async fn empty_codenames_refuse_to_mount() {
        let (p, t) = (pool().await, tera());
        let msg = refusal(|| {
            DeleteView::for_model(Post::SCHEMA)
                .permissions(&[])
                .router("/posts", t, p)
        })
        .expect("empty codenames must not mount");
        assert!(msg.contains("login_required"), "{msg}");
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
        let bulk = || {
            Request::builder()
                .method(Method::POST)
                .uri("/posts")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
                .body(Body::from(format!(
                    "_csrf={CSRF}&action=delete_selected&_selected_action=7"
                )))
                .unwrap()
        };
        let res = app.clone().oneshot(as_user(bulk(), 5)).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(rows(&p).await, 1);

        grant(&p, &["change", "delete"]).await;
        let res = app.oneshot(as_user(bulk(), 5)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(rows(&p).await, 0);
    }
}

/// `tenant_router`: codenames are read in the routed tenant's database,
/// for a credential bound to that tenant only.
#[cfg(feature = "tenancy")]
mod tenant {
    use super::gated::{of_acme, signed_in};
    use super::*;
    use rustango::extractors::TenantContext;
    use rustango::tenancy::{
        session::SessionSecret, ChainResolver, Org, OrgResolver, TenancyError, TenantPools,
    };

    #[derive(Clone)]
    struct Acme(Org);

    #[async_trait::async_trait]
    impl OrgResolver for Acme {
        async fn resolve(
            &self,
            _parts: &axum::http::request::Parts,
            _registry: &Pool,
        ) -> Result<Option<Org>, TenancyError> {
            Ok(Some(self.0.clone()))
        }
    }

    async fn context(org: Org) -> Arc<TenantContext<sqlx::Sqlite>> {
        let registry = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        Arc::new(TenantContext::<sqlx::Sqlite> {
            pools: Arc::new(TenantPools::new(registry)),
            resolver: ChainResolver::new().push(Acme(org)),
            session_secret: SessionSecret::from_bytes(CSRF.as_bytes().to_vec()),
            operator_secret: SessionSecret::from_bytes(CSRF.as_bytes().to_vec()),
        })
    }

    /// A fixed-pool view inside a tenancy app: its signed-in users are the
    /// tenants', so a mounted tenant context alone makes them refused.
    #[tokio::test]
    async fn a_fixed_pool_view_under_a_tenant_context_refuses_users() {
        let p = pool().await;
        let ctx = context(rustango::testkit::org_with(|o| o.id = Auto::Set(1))).await;
        let app = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .router("/posts", tera(), p.clone())
            .layer(axum::Extension(ctx));
        let res = app
            .oneshot(signed_in(delete_post(), 5, true))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(rows(&p).await, 1);
    }

    /// A gated tenant DeleteView over acme's own file database, user 5 in it.
    async fn app() -> (axum::Router, Pool) {
        let path =
            std::env::temp_dir().join(format!("rustango_tv_access_{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let sq = sqlx::SqlitePool::connect(&url).await.unwrap();
        for sql in [
            "CREATE TABLE tv_access_post (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
            "INSERT INTO tv_access_post (id, title) VALUES (7, 'seven')",
        ] {
            sqlx::query(sql).execute(&sq).await.unwrap();
        }
        let pool = Pool::Sqlite(sq.clone());
        rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
            .await
            .unwrap();
        rustango::testkit::migrate_framework(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO rustango_users (id, username, password_hash, is_superuser, active, \
             created_at) VALUES (5, 'ada', '', 0, 1, datetime('now'))",
        )
        .execute(&sq)
        .await
        .unwrap();
        let org = rustango::testkit::org_with(|o| {
            o.id = Auto::Set(1);
            o.slug = "acme".into();
            o.storage_mode = "database".into();
            o.backend_kind = "sqlite".into();
            o.database_url = Some(url);
        });
        let ctx = context(org).await;
        let router = DeleteView::for_model(Post::SCHEMA)
            .permissions_for_model()
            .tenant_router("/posts", tera())
            .layer(axum::Extension(ctx));
        (router, pool)
    }

    #[tokio::test]
    async fn the_tenant_gate_binds_the_credential_to_the_tenant() {
        let (app, pool) = app().await;
        rustango::tenancy::permissions::set_user_perm_pool(5, "tv_access_post.delete", true, &pool)
            .await
            .unwrap();

        // Bound to another tenant, or to none: refused.
        let mut foreign = signed_in(delete_post(), 5, false);
        foreign
            .extensions_mut()
            .insert(rustango::tenancy::TenantSlug::new(2, "globex"));
        let res = app.clone().oneshot(foreign).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "foreign tenant");
        let res = app
            .clone()
            .oneshot(signed_in(delete_post(), 5, false))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "untenanted");
        assert_eq!(rows(&pool).await, 1);

        // Bound to acme and granted in acme's database: deletes.
        let res = app.oneshot(of_acme(delete_post(), 5, false)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(rows(&pool).await, 0);
    }
}
