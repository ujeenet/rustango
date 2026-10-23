//! Admin permission edges on every backend: a gate's extension session,
//! a locked flag on an inline add, and a child table the user cannot view.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::admin::AdminSession;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "aae_widget")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub flag: bool,
}

rustango::register_admin_superuser_fields!(Widget, Change, [flag]);

#[derive(Model, Debug, Clone)]
#[rustango(table = "aae_order")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aae_line")]
#[allow(dead_code)]
pub struct Line {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aae_order", on = "id")]
    pub order_id: i64,
    pub qty: i64,
    pub flag: bool,
}

rustango::register_admin_superuser_fields!(Line, Add, [flag]);

rustango::register_admin_inline!(
    parent = "aae_order",
    child = "aae_line",
    fk = "order_id",
    extra = 1,
);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    fresh_table::<Widget>(pool).await;
    drop_table(pool, "aae_line").await;
    fresh_table::<Order>(pool).await;
    fresh_table::<Line>(pool).await;
}

async fn send(app: axum::Router, method: &str, uri: &str, form: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn admin(pool: &Pool) -> rustango::admin::Builder {
    rustango::admin::Builder::new(pool.clone()).admin_prefix("")
}

fn perms(list: &[&str]) -> Vec<String> {
    list.iter().map(|p| (*p).to_owned()).collect()
}

async fn widget(pool: &Pool) -> i64 {
    let mut w = Widget {
        id: Auto::default(),
        name: "w".into(),
        flag: false,
    };
    w.insert_pool(pool).await.expect("widget");
    *w.id.get().expect("pk")
}

async fn flag_of(pool: &Pool, id: i64) -> bool {
    Widget::objects()
        .fetch(pool)
        .await
        .expect("widgets")
        .into_iter()
        .find(|w| w.id.get() == Some(&id))
        .expect("widget")
        .flag
}

/// A custom gate that sets only `Extension<AdminSession>` is a signed-in
/// non-superuser, not "no login".
async fn extension_session_locks_superuser_fields(pool: &Pool) {
    let id = widget(pool).await;
    let member = admin(pool)
        .build()
        .layer(axum::Extension(AdminSession::new(7, "m", false)));
    let (status, body) = send(member, "POST", &format!("/aae_widget/{id}"), "name=x&flag=on").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(!flag_of(pool, id).await, "a member must not set the flag");
    // The control: no login is full access.
    let (status, body) = send(admin(pool).build(), "POST", &format!("/aae_widget/{id}"), "name=x&flag=on").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(flag_of(pool, id).await);
}

async fn order(pool: &Pool) -> i64 {
    let mut o = Order {
        id: Auto::default(),
        name: "o".into(),
    };
    o.insert_pool(pool).await.expect("order");
    *o.id.get().expect("pk")
}

/// A locked NOT NULL flag defaults on an inline add, as on the create form.
async fn inline_add_defaults_a_locked_flag(pool: &Pool) {
    let o = order(pool).await;
    let app = admin(pool)
        .with_user_perms(perms(&[
            "aae_order.view",
            "aae_order.change",
            "aae_line.view",
            "aae_line.add",
            "aae_line.change",
        ]))
        .build();
    let form = "name=o&aae_line-TOTAL_FORMS=1&aae_line-INITIAL_FORMS=0\
                &aae_line-0-qty=3&aae_line-0-flag=on";
    let (status, body) = send(app, "POST", &format!("/aae_order/{o}"), form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let lines = Line::objects().fetch(pool).await.expect("lines");
    let line = lines.iter().find(|l| l.order_id == o).expect("added line");
    assert_eq!(line.qty, 3);
    assert!(!line.flag, "a member must not set the flag");
}

/// Without `{child}.view` the parent's pages show no child rows, and an
/// inline POST writes none.
async fn hidden_child_is_not_rendered_or_written(pool: &Pool) {
    let o = order(pool).await;
    let mut l = Line {
        id: Auto::default(),
        order_id: o,
        qty: 4242,
        flag: false,
    };
    l.insert_pool(pool).await.expect("line");
    let parent_only = || {
        admin(pool)
            .with_user_perms(perms(&["aae_order.view", "aae_order.change"]))
            .build()
    };
    for uri in [format!("/aae_order/{o}"), format!("/aae_order/{o}/edit")] {
        let (status, html) = send(parent_only(), "GET", &uri, "").await;
        assert_eq!(status, StatusCode::OK, "{uri}: {html}");
        assert!(!html.contains("4242"), "{uri} shows a hidden child row");
        assert!(!html.contains("aae_line-TOTAL_FORMS"), "{uri}");
    }
    let form = "name=o&aae_line-TOTAL_FORMS=1&aae_line-INITIAL_FORMS=0&aae_line-0-qty=7";
    let (status, body) = send(parent_only(), "POST", &format!("/aae_order/{o}"), form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let lines = Line::objects().fetch(pool).await.expect("lines");
    assert_eq!(lines.iter().filter(|l| l.order_id == o).count(), 1);
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        extension_session_locks_superuser_fields,
        inline_add_defaults_a_locked_flag,
        hidden_child_is_not_rendered_or_written,
    ],
);
