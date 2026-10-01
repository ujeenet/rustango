//! `register_admin_queryset!` scopes by-pk reads, facet counts and FK
//! facet labels on every backend (#1859, #2029); inlines hide secrets and
//! rows the view hook refuses, cap rows and insert natural PKs (#1861, #1717).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_item",
    display = "title",
    admin(list_display = "title", list_filter = "owner_id")
)]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub owner_id: i64,
}

fn owner_one(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("owner_id", Op::Eq, SqlValue::I64(1))]
}
rustango::register_admin_queryset!("rowscope_item", owner_one);

/// An FK facet onto `rowscope_item`, whose hook hides owner 77.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_note",
    admin(list_display = "body", list_filter = "item_id")
)]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub body: String,
    #[rustango(fk = "rowscope_item", on = "id")]
    pub item_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rowscope_parent", display = "name")]
#[allow(dead_code)]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

/// A natural-PK child with a secret; the view hook hides `hidden` rows.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_child",
    admin(formfield_overrides = "secret:password")
)]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key, max_length = 16)]
    pub code: String,
    pub parent_id: i64,
    #[rustango(max_length = 64)]
    pub label: String,
    #[rustango(max_length = 64)]
    pub secret: String,
    /// Nullable, so an inline insert that omits it still writes.
    pub hidden: Option<bool>,
}

rustango::register_admin_inline!(
    parent = "rowscope_parent",
    child = "rowscope_child",
    fk = "parent_id",
    fields = &["code", "label", "secret"],
    extra = 1,
    max_num = Some(2),
);

fn not_hidden(_: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    row.and_then(|r| r.get("hidden"))
        .is_none_or(|v| !(v == &serde_json::json!(true) || v == &serde_json::json!(1)))
}
rustango::register_admin_object_permission!("rowscope_child", "view", not_hidden);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "rowscope_note").await;
    fresh_table::<Item>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Parent>(pool).await;
    fresh_table::<Child>(pool).await;
}

async fn post(pool: &Pool, uri: &str, form: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(pool: &Pool, uri: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn seed(pool: &Pool, title: &str, owner_id: i64) -> i64 {
    let mut item = Item {
        id: Auto::default(),
        title: title.into(),
        owner_id,
    };
    item.insert_pool(pool).await.expect("insert");
    *item.id.get().expect("pk")
}

async fn hidden_rows_are_404_and_uncounted(pool: &Pool) {
    let mine = seed(pool, "mine", 1).await;
    let theirs = seed(pool, "theirs", 77).await;
    assert_eq!(
        get(pool, &format!("/rowscope_item/{mine}")).await.0,
        StatusCode::OK
    );
    let (status, body) = get(pool, &format!("/rowscope_item/{theirs}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = get(pool, "/rowscope_item").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("owner_id=1"), "own facet: {body}");
    assert!(!body.contains("owner_id=77"), "scoped facet: {body}");
}

/// An FK facet labels a hidden target by its key, not its title (#2029).
async fn fk_facet_hides_a_hidden_targets_name(pool: &Pool) {
    let theirs = seed(pool, "secret-title", 77).await;
    let mut note = Note {
        id: Auto::default(),
        body: "n".into(),
        item_id: theirs,
    };
    note.insert_pool(pool).await.expect("insert note");
    let (status, body) = get(pool, "/rowscope_note").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(&format!("item_id={theirs}")), "facet: {body}");
    assert!(!body.contains("secret-title"), "hidden name leaked: {body}");
}

async fn seed_child(pool: &Pool, code: &str, parent_id: i64, label: &str, hidden: bool) {
    let c = Child {
        code: code.into(),
        parent_id,
        label: label.into(),
        secret: format!("pw-{code}"),
        hidden: Some(hidden),
    };
    c.insert_pool(pool).await.expect("insert child");
}

async fn seed_parent(pool: &Pool) -> i64 {
    let mut p = Parent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("insert parent");
    *p.id.get().expect("pk")
}

async fn child(pool: &Pool, code: &str) -> Option<Child> {
    Child::objects()
        .filter("code", code)
        .fetch(pool)
        .await
        .expect("fetch child")
        .into_iter()
        .next()
}

/// Detail and edit inlines never echo a secret or a row the view hook refuses.
async fn inlines_hide_secrets_and_refused_rows(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "shown-label", false).await;
    seed_child(pool, "b", p, "ghost-label", true).await;
    for uri in [
        format!("/rowscope_parent/{p}"),
        format!("/rowscope_parent/{p}/edit"),
    ] {
        let (status, body) = get(pool, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("shown-label"), "{uri}: {body}");
        assert!(!body.contains("pw-a"), "{uri} leaks the secret: {body}");
        assert!(
            !body.contains("ghost-label"),
            "{uri} shows a refused row: {body}"
        );
    }
}

/// An empty secret keeps the stored one; a typed natural PK inserts.
async fn inline_post_keeps_secrets_and_inserts_natural_pks(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "old", false).await;
    let form = "name=p&rowscope_child-TOTAL_FORMS=2&rowscope_child-INITIAL_FORMS=1\
        &rowscope_child-0-code=a&rowscope_child-0-label=new&rowscope_child-0-secret=\
        &rowscope_child-1-code=n1&rowscope_child-1-label=added&rowscope_child-1-secret=s";
    let (status, body) = post(pool, &format!("/rowscope_parent/{p}"), form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let a = child(pool, "a").await.expect("a");
    assert_eq!((a.label.as_str(), a.secret.as_str()), ("new", "pw-a"));
    let n1 = child(pool, "n1").await.expect("natural PK row inserted");
    assert_eq!((n1.parent_id, n1.label.as_str()), (p, "added"));
}

/// A POST that adds rows past `max_num` is refused and writes nothing.
async fn inline_post_enforces_max_num(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "one", false).await;
    seed_child(pool, "b", p, "two", false).await;
    let form = "name=p&rowscope_child-TOTAL_FORMS=3&rowscope_child-INITIAL_FORMS=2\
        &rowscope_child-0-code=a&rowscope_child-0-label=one\
        &rowscope_child-1-code=b&rowscope_child-1-label=two\
        &rowscope_child-2-code=c&rowscope_child-2-label=three&rowscope_child-2-secret=s";
    let (status, body) = post(pool, &format!("/rowscope_parent/{p}"), form).await;
    assert_eq!(status, StatusCode::OK, "re-rendered form: {body}");
    assert!(body.contains("at most 2 rows"), "{body}");
    assert!(
        child(pool, "c").await.is_none(),
        "row past max_num was written"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        hidden_rows_are_404_and_uncounted,
        fk_facet_hides_a_hidden_targets_name,
        inlines_hide_secrets_and_refused_rows,
        inline_post_keeps_secrets_and_inserts_natural_pks,
        inline_post_enforces_max_num,
    ],
}
