//! A tenant ViewSet's hooks see a credential only on the tenant it is bound
//! to, whether the tenant is routed by its own host or by an apex path prefix.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, HOST};
use axum::http::{Request, StatusCode};
use axum::Router;
use rustango::core::Model as _;
use rustango::extractors::DatabaseTenantContext;
use rustango::sql::FetcherPool as _;
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::auth_backends::{AuthBackend, ModelBackend};
use rustango::tenancy::middleware::AuthenticatedUser;
use rustango::tenancy::User;
use rustango::tenancy::{
    session::SessionSecret, BackendKind, ChainResolver, DatabasePools, Org, OrgResolver,
    RouterAuthExt, TenancyError, TenantSlug,
};
use rustango::viewset::ViewSet;
use tower::ServiceExt as _;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vopt_note")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub title: String,
}

/// Only a superuser of this tenant sees a note.
fn superuser_view(parts: &axum::http::request::Parts, _: Option<&serde_json::Value>) -> bool {
    rustango::object_permissions::is_superuser(parts)
}
rustango::register_object_permission!("vopt_note", "view", superuser_view);

fn org(slug: &str) -> Org {
    Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: None,
        ..rustango::testkit::org()
    }
}

/// `<slug>.localhost` is a tenant's own host; `/<slug>/…` on the apex is a path prefix.
struct HostOrPrefix;

#[async_trait::async_trait]
impl OrgResolver for HostOrPrefix {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, TenancyError> {
        let host = parts
            .headers
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if let Some(slug) = host.strip_suffix(".localhost") {
            return Ok(Some(org(slug)));
        }
        let first = parts.uri.path().trim_start_matches('/').split('/').next();
        Ok(first.filter(|s| !s.is_empty() && *s != "api").map(org))
    }
}

/// One router: the host mount at `/api/notes`, the prefix mounts at `/<slug>/api/notes`.
fn app(ns: &str, outer: Option<(AuthenticatedUser, &str)>) -> (Router, String) {
    let template = format!("sqlite:file:vopt_{ns}_{{slug}}?mode=memory&cache=shared");
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver: ChainResolver::new().push(HostOrPrefix),
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });
    let vs = || ViewSet::for_model(Note::SCHEMA, &["id", "title"]).allow_anonymous();
    let users =
        || ViewSet::for_model(User::SCHEMA, &["id", "username", "is_superuser"]).allow_anonymous();
    let mut router = Router::new()
        .merge(vs().tenant_router("/api/notes"))
        .merge(vs().tenant_router("/alpha/api/notes"))
        .merge(vs().tenant_router("/beta/api/notes"))
        .merge(users().tenant_router("/api/users"))
        .merge(users().tenant_router("/beta/api/users"));
    router = match outer {
        // An app layer that already decided who the caller is, for one tenant.
        Some((user, slug)) => router
            .layer(axum::Extension(user))
            .layer(axum::Extension(TenantSlug(slug.to_owned()))),
        None => {
            let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend)];
            router.require_auth(backends)
        }
    };
    (router.layer(axum::Extension(ctx)), template)
}

/// A tenant database with one note and one superuser `ann`.
async fn tenant(url: &str) -> rustango::sql::Pool {
    let pool = rustango::sql::Pool::connect(url).await.expect("tenant db");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("users");
    rustango::testkit::create_tables_for::<Note>(&pool)
        .await
        .expect("notes");
    let mut ann = rustango::tenancy::User {
        username: "ann".into(),
        password_hash: rustango::tenancy::password::hash("s3cret").expect("hash"),
        is_superuser: true,
        ..rustango::testkit::user()
    };
    ann.insert_pool(&pool).await.expect("ann");
    let mut bob = rustango::tenancy::User {
        username: "bob".into(),
        ..rustango::testkit::user()
    };
    bob.insert_pool(&pool).await.expect("bob");
    let mut note = Note {
        id: Auto::default(),
        title: "n".into(),
    };
    note.insert_pool(&pool).await.expect("note");
    pool
}

/// `count` of a list, or the status when it is not a 200.
async fn count(
    app: &Router,
    host: &str,
    path: &str,
    basic: Option<&str>,
) -> Result<i64, StatusCode> {
    use base64::Engine as _;
    let mut req = Request::builder().uri(path).header(HOST, host);
    if let Some(b) = basic {
        let enc = base64::engine::general_purpose::STANDARD.encode(b);
        req = req.header(AUTHORIZATION, format!("Basic {enc}"));
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    if res.status() != StatusCode::OK {
        return Err(res.status());
    }
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    Ok(body["count"].as_i64().expect("count"))
}

/// A superuser of `alpha` is one there, by host or by prefix, and refused on `beta`.
#[tokio::test]
async fn a_credential_counts_only_on_its_own_tenant() {
    let su = AuthenticatedUser {
        id: 1,
        username: "ann".into(),
        is_superuser: true,
    };
    let (app, template) = app("outer", Some((su, "alpha")));
    let _alpha = tenant(&template.replace("{slug}", "alpha")).await;
    let _beta = tenant(&template.replace("{slug}", "beta")).await;
    // Own host, then apex + path prefix.
    assert_eq!(
        count(&app, "alpha.localhost", "/api/notes", None).await,
        Ok(1)
    );
    assert_eq!(
        count(&app, "apex.test", "/alpha/api/notes", None).await,
        Ok(1)
    );
    assert_eq!(
        count(&app, "beta.localhost", "/api/notes", None).await,
        Err(StatusCode::FORBIDDEN)
    );
    assert_eq!(
        count(&app, "apex.test", "/beta/api/notes", None).await,
        Err(StatusCode::FORBIDDEN)
    );
}

/// A credential bound to `alpha` is refused on `beta`, by host or prefix:
/// it cannot grant superuser there through the field rules.
#[tokio::test]
async fn a_credential_for_another_tenant_cannot_write() {
    let su = AuthenticatedUser {
        id: 1,
        username: "ann".into(),
        is_superuser: true,
    };
    let (app, template) = app("write", Some((su, "alpha")));
    let _alpha = tenant(&template.replace("{slug}", "alpha")).await;
    let beta = tenant(&template.replace("{slug}", "beta")).await;
    let bob = User::objects()
        .filter("username", "bob")
        .fetch(&beta)
        .await
        .expect("bob")
        .remove(0);
    let id = *bob.id.get().expect("pk");
    for (host, path) in [
        ("beta.localhost", format!("/api/users/{id}")),
        ("apex.test", format!("/beta/api/users/{id}")),
    ] {
        let req = Request::builder()
            .method("PATCH")
            .uri(&path)
            .header(HOST, host)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"is_superuser": true}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{host}{path}");
    }
    let bob = User::objects()
        .filter("username", "bob")
        .fetch(&beta)
        .await
        .expect("bob")
        .remove(0);
    assert!(!bob.is_superuser, "alpha's superuser promoted a beta user");
}

/// The control through `require_auth`: the tenant's own superuser sees its note.
#[tokio::test]
async fn an_authenticated_superuser_sees_its_tenants_rows() {
    let (app, template) = app("auth", None);
    let _alpha = tenant(&template.replace("{slug}", "alpha")).await;
    let basic = Some("ann:s3cret");
    assert_eq!(
        count(&app, "alpha.localhost", "/api/notes", basic).await,
        Ok(1)
    );
    assert_eq!(
        count(&app, "apex.test", "/alpha/api/notes", basic).await,
        Ok(1)
    );
}
