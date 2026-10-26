//! A ViewSet keeps the admin's superuser-only fields locked, on every backend.

#![cfg(all(feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::core::Column as _;
use rustango::core::Model as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::middleware::AuthenticatedUser;
use rustango::tenancy::User;
use rustango::tri_dialect_test;
use rustango::viewset::ViewSet;
use tower::ServiceExt as _;

/// A model with an admin `add` hook, to check the ViewSet runs it.
#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vsf_hooked")]
#[allow(dead_code)]
pub struct Hooked {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 32)]
    pub label: String,
}

/// Denies a request with `x-deny`, and a row labelled `blocked`.
fn add_hook(parts: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    !parts.headers.contains_key("x-deny") && row.is_none_or(|r| r["label"] != "blocked")
}
rustango::register_admin_object_permission!("vsf_hooked", "add", add_hook);

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    rustango::testkit::matrix::fresh_table::<Hooked>(pool).await;
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

/// `/users` over `rustango_users`, the caller signed in as `superuser` or not.
fn app(pool: &Pool, superuser: bool) -> axum::Router {
    anonymous(pool).layer(axum::Extension(AuthenticatedUser {
        id: 1,
        username: "caller".into(),
        is_superuser: superuser,
    }))
}

/// `/users` with no `AuthenticatedUser` at all. The superuser-only fields
/// are listed, so only the superuser rule keeps a member from writing them.
fn anonymous(pool: &Pool) -> axum::Router {
    let fields = &[
        "id",
        "username",
        "password_hash",
        "is_superuser",
        "active",
        "created_at",
        "data",
    ];
    ViewSet::for_model(User::SCHEMA, fields)
        .allow_anonymous()
        .router_pool("/users", pool.clone())
}

/// A full user body that asks for superuser and a new hash.
fn full_body(username: &str) -> serde_json::Value {
    serde_json::json!({
        "username": username, "password_hash": "new-hash", "is_superuser": true,
        "active": true, "created_at": "2024-01-01T00:00:00Z", "data": {}
    })
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let res = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn by_name(pool: &Pool, username: &str) -> User {
    User::objects()
        .where_(User::username.eq(username.to_owned()))
        .fetch(pool)
        .await
        .expect("fetch")
        .pop()
        .expect("user")
}

async fn seed(pool: &Pool) -> (i64, String) {
    let mut u = User {
        username: unique("vsu"),
        password_hash: "old-hash".into(),
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed");
    (*u.id.get().expect("pk"), u.username)
}

async fn a_member_cannot_grant_superuser_or_set_a_hash(pool: &Pool) {
    let (id, _) = seed(pool).await;
    let renamed = unique("vsr");
    let body = serde_json::json!({
        "username": renamed, "is_superuser": true, "password_hash": "new-hash"
    });
    let (status, res) = send(app(pool, false), "PATCH", &format!("/users/{id}"), body).await;
    assert!(status.is_success(), "{status}: {res}");
    let u = by_name(pool, &renamed).await;
    assert!(!u.is_superuser, "is_superuser stays locked");
    assert_eq!(u.password_hash, "old-hash");
}

async fn a_member_cannot_create_a_superuser(pool: &Pool) {
    let name = unique("vsc");
    let body = serde_json::json!({
        "username": name, "password_hash": "h", "is_superuser": true,
        "active": true, "created_at": "2024-01-01T00:00:00Z", "data": {}
    });
    let (status, res) = send(app(pool, false), "POST", "/users", body).await;
    assert_eq!(status, StatusCode::CREATED, "{res}");
    assert!(!by_name(pool, &name).await.is_superuser);
}

async fn a_member_put_keeps_them(pool: &Pool) {
    let (id, _) = seed(pool).await;
    let renamed = unique("vsp");
    let (status, res) = send(
        app(pool, false),
        "PUT",
        &format!("/users/{id}"),
        full_body(&renamed),
    )
    .await;
    assert!(status.is_success(), "{status}: {res}");
    let u = by_name(pool, &renamed).await;
    assert!(!u.is_superuser);
    assert_eq!(u.password_hash, "old-hash");
}

async fn a_member_bulk_create_keeps_them(pool: &Pool) {
    let (a, b) = (unique("vsba"), unique("vsbb"));
    let body = serde_json::json!([full_body(&a), full_body(&b)]);
    let (status, res) = send(app(pool, false), "POST", "/users", body).await;
    assert!(status.is_success(), "{status}: {res}");
    assert!(!by_name(pool, &a).await.is_superuser);
    assert!(!by_name(pool, &b).await.is_superuser);
}

/// No principal at all fails closed.
async fn an_anonymous_write_keeps_them(pool: &Pool) {
    let (id, _) = seed(pool).await;
    let renamed = unique("vsn");
    let body = serde_json::json!({ "username": renamed, "is_superuser": true });
    let (status, res) = send(anonymous(pool), "PATCH", &format!("/users/{id}"), body).await;
    assert!(status.is_success(), "{status}: {res}");
    assert!(!by_name(pool, &renamed).await.is_superuser);
    let name = unique("vsnc");
    let (status, res) = send(anonymous(pool), "POST", "/users", full_body(&name)).await;
    assert_eq!(status, StatusCode::CREATED, "{res}");
    assert!(!by_name(pool, &name).await.is_superuser);
}

/// A body of only locked fields answers like an update that changed nothing.
async fn a_locked_only_patch_is_a_no_op(pool: &Pool) {
    let (id, name) = seed(pool).await;
    let body = serde_json::json!({ "is_superuser": true, "password_hash": "new-hash" });
    let (status, res) = send(app(pool, false), "PATCH", &format!("/users/{id}"), body).await;
    assert_eq!(status, StatusCode::OK, "{res}");
    let u = by_name(pool, &name).await;
    assert!(!u.is_superuser);
    assert_eq!(u.password_hash, "old-hash");
}

/// The control: a superuser principal writes them.
async fn a_superuser_writes_them(pool: &Pool) {
    let (id, name) = seed(pool).await;
    let body = serde_json::json!({ "is_superuser": true, "password_hash": "new-hash" });
    let (status, res) = send(app(pool, true), "PATCH", &format!("/users/{id}"), body).await;
    assert!(status.is_success(), "{status}: {res}");
    let u = by_name(pool, &name).await;
    assert!(u.is_superuser);
    assert_eq!(u.password_hash, "new-hash");
}

/// Opted in, a create runs the admin's `add` hooks: on the request, then on
/// each written row, single or bulk; a denied row writes nothing. By
/// default it does not.
async fn create_runs_the_admin_add_hooks(pool: &Pool) {
    let vs = || ViewSet::for_model(Hooked::SCHEMA, &["id", "label"]).allow_anonymous();
    let default_app = vs().router_pool("/hooked", pool.clone());
    let app = vs().admin_add_hooks().router_pool("/hooked", pool.clone());
    let post_to = |app: &axum::Router, body: serde_json::Value, deny: bool| {
        let mut req = Request::builder()
            .method("POST")
            .uri("/hooked")
            .header("content-type", "application/json");
        if deny {
            req = req.header("x-deny", "1");
        }
        app.clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
    };
    let post = |body, deny| post_to(&app, body, deny);
    let ok = serde_json::json!({ "label": "ok" });
    let res = post(ok.clone(), true).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = post(serde_json::json!({ "label": "blocked" }), false)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let bulk = serde_json::json!([{ "label": "first" }, { "label": "blocked" }]);
    let res = post(bulk, false).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let labels = || async {
        Hooked::objects()
            .fetch(pool)
            .await
            .expect("fetch")
            .into_iter()
            .map(|h| h.label)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    };
    assert!(labels().await.is_empty(), "a denied create wrote a row");
    let res = post(ok, false).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(labels().await, ["ok"]);
    // Not opted in: the hooks do not apply.
    let blocked = serde_json::json!({ "label": "blocked" });
    let res = post_to(&default_app, blocked, true).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(labels().await, ["blocked", "ok"]);
}

/// A write echoes its row only to a caller who may retrieve it; anyone
/// else gets the new pk, and a create its `Location`.
async fn a_write_echoes_only_with_the_view_perm(pool: &Pool) {
    use rustango::tenancy::permissions::set_user_perm_pool;
    use rustango::viewset::ViewSetPerms;
    let (uid, _) = seed(pool).await;
    for cn in ["vsf_hooked.add", "vsf_hooked.change"] {
        set_user_perm_pool(uid, cn, true, pool)
            .await
            .expect("grant");
    }
    let app = || {
        let cn = |c: &str| vec![format!("vsf_hooked.{c}")];
        ViewSet::for_model(Hooked::SCHEMA, &["id", "label"])
            .permissions(ViewSetPerms {
                list: cn("view"),
                retrieve: cn("view"),
                create: cn("add"),
                update: cn("change"),
                destroy: cn("delete"),
            })
            .router_pool("/hooked", pool.clone())
            .layer(axum::Extension(AuthenticatedUser {
                id: uid,
                username: "member".into(),
                is_superuser: false,
            }))
    };
    let nested = axum::Router::new().nest("/api", app());
    let res = nested
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/hooked")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"label":"a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let id = Hooked::objects().fetch(pool).await.expect("fetch")[0]
        .id
        .get()
        .copied()
        .expect("pk");
    assert_eq!(
        res.headers().get("location").and_then(|v| v.to_str().ok()),
        Some(format!("/api/hooked/{id}").as_str())
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let only_pk = serde_json::json!({ "id": id });
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body, only_pk, "create echoed more than the pk");
    let patch = serde_json::json!({ "label": "b" });
    let (status, body) = send(app(), "PATCH", &format!("/hooked/{id}"), patch.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        only_pk
    );
    let bulk = serde_json::json!([{ "label": "c" }]);
    let (status, body) = send(app(), "POST", "/hooked", bulk).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
    let keys: Vec<_> = rows[0].as_object().expect("pk object").keys().collect();
    assert_eq!(keys, ["id"], "{body}");

    set_user_perm_pool(uid, "vsf_hooked.view", true, pool)
        .await
        .expect("grant view");
    let (status, body) = send(app(), "PATCH", &format!("/hooked/{id}"), patch).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"b\""), "{body}");
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        a_write_echoes_only_with_the_view_perm,
        create_runs_the_admin_add_hooks,
        a_member_cannot_grant_superuser_or_set_a_hash,
        a_member_cannot_create_a_superuser,
        a_member_put_keeps_them,
        a_member_bulk_create_keeps_them,
        an_anonymous_write_keeps_them,
        a_locked_only_patch_is_a_no_op,
        a_superuser_writes_them,
    ],
);
