//! Admin write errors on every backend: a bad action is a 400 (#2346);
//! a refused write shows a plain message, never the driver's text (#2345);
//! deleting a referenced row is a 409 naming a visible referrer (#2340); a bad
//! inline row re-renders the form and saves no inline row (#2339).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "wrerr_parent",
    display = "name",
    admin(list_display = "name", actions = "delete_selected")
)]
#[allow(dead_code)]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32, unique)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "wrerr_child", admin(list_display = "note"))]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "wrerr_parent", on = "id")]
    pub parent_id: i64,
    pub qty: i32,
    #[rustango(max_length = 32, unique)]
    pub note: String,
}

/// A typed PK beside a unique field (correctness-002).
#[derive(Model, Debug, Clone)]
#[rustango(table = "wrerr_coded", admin(list_display = "slug"))]
#[allow(dead_code)]
pub struct Coded {
    #[rustango(primary_key, max_length = 16)]
    pub code: String,
    #[rustango(max_length = 32, unique)]
    pub slug: String,
}

rustango::register_admin_inline!(
    parent = "wrerr_parent",
    child = "wrerr_child",
    fk = "parent_id",
    fields = &["qty", "note"],
    extra = 2,
);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "wrerr_child").await;
    fresh_table::<Parent>(pool).await;
    fresh_table::<Child>(pool).await;
    fresh_table::<Coded>(pool).await;
}

async fn post(pool: &Pool, uri: &str, form: &str) -> (StatusCode, String) {
    post_to(rustango::admin::Builder::new(pool.clone()), uri, form).await
}

async fn post_to(admin: rustango::admin::Builder, uri: &str, form: &str) -> (StatusCode, String) {
    let app = admin.admin_prefix("").build();
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

async fn seed_parent(pool: &Pool, name: &str) -> i64 {
    let mut p = Parent {
        id: Auto::default(),
        name: name.into(),
    };
    p.insert_pool(pool).await.expect("insert parent");
    *p.id.get().expect("pk")
}

/// An action the model does not list is the client's error (#2346).
async fn unknown_action_is_a_400(pool: &Pool) {
    let p = seed_parent(pool, "p").await;
    let (status, body) = post(
        pool,
        "/wrerr_parent/__action",
        &format!("action=nope&_selected={p}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// What a driver message would put on the page.
fn assert_no_driver_text(body: &str) {
    for raw in [
        "violates",
        "constraint failed",
        "Duplicate entry",
        "foreign key constraint",
        "wrerr_parent_name_key",
        "wrerr_parent.name",
        "wrerr_child_parent_id_fkey",
    ] {
        assert!(!body.contains(raw), "driver text `{raw}` leaked");
    }
    assert!(body.contains("error id "), "no correlation id");
}

/// A duplicate on create and on edit names the field, not the constraint.
async fn unique_refusal_is_a_plain_message(pool: &Pool) {
    seed_parent(pool, "taken").await;
    let other = seed_parent(pool, "other").await;
    for uri in ["/wrerr_parent".to_owned(), format!("/wrerr_parent/{other}")] {
        let (status, body) = post(pool, &uri, "name=taken").await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_no_driver_text(&body);
        assert!(
            body.contains("A Parent with this name already exists."),
            "{uri}: {body}"
        );
    }
}

/// A clash on a typed PK does not blame the unique field.
async fn unique_refusal_on_a_typed_pk_names_no_field(pool: &Pool) {
    let (status, body) = post(pool, "/wrerr_coded", "code=a&slug=one").await;
    assert!(status.is_redirection(), "{status}: {body}");
    let (status, body) = post(pool, "/wrerr_coded", "code=a&slug=two").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("this slug"), "{body}");
    assert!(
        body.contains("A Coded with these values already exists."),
        "{body}"
    );
}

/// A missing FK target is a plain message too.
async fn fk_refusal_is_a_plain_message(pool: &Pool) {
    let (status, body) = post(pool, "/wrerr_child", "parent_id=9999&qty=1&note=n").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_no_driver_text(&body);
    assert!(body.contains("does not exist"), "{body}");
}

async fn seed_child(pool: &Pool, parent_id: i64, note: &str) {
    let mut c = Child {
        id: Auto::default(),
        parent_id,
        qty: 1,
        note: note.into(),
    };
    c.insert_pool(pool).await.expect("insert child");
}

async fn parent_count(pool: &Pool) -> usize {
    Parent::objects().fetch(pool).await.expect("fetch").len()
}

/// Single delete and `delete_selected` of a referenced row: 409, row kept.
async fn deleting_a_referenced_row_is_a_409(pool: &Pool) {
    let p = seed_parent(pool, "p").await;
    seed_child(pool, p, "c").await;
    for (uri, form) in [
        (format!("/wrerr_parent/{p}/delete"), String::new()),
        (
            "/wrerr_parent/__action".to_owned(),
            format!("action=delete_selected&_selected={p}"),
        ),
    ] {
        let (status, body) = post(pool, &uri, &form).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
        // Only PG's error names the table; elsewhere the 409 hedges.
        let by = if pool.backend_name() == "postgres" {
            "by rows in wrerr_child"
        } else {
            "by other rows, possibly in wrerr_child"
        };
        assert!(body.contains(by), "{uri}: {body}");
        assert_eq!(parent_count(pool).await, 1, "{uri}");
    }
    // A table the user cannot open in the admin is not named.
    let hidden = rustango::admin::Builder::new(pool.clone()).show_only(["wrerr_parent"]);
    let (status, body) = post_to(hidden, &format!("/wrerr_parent/{p}/delete"), "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!body.contains("wrerr_child"), "{body}");
    assert!(body.contains("other rows"), "{body}");
    // Control: an unreferenced row still goes.
    let q = seed_parent(pool, "q").await;
    let (status, body) = post(pool, &format!("/wrerr_parent/{q}/delete"), "").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(parent_count(pool).await, 1);
}

async fn notes(pool: &Pool) -> Vec<String> {
    let mut v: Vec<String> = Child::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .map(|c| c.note)
        .collect();
    v.sort();
    v
}

fn inline_form(rows: &[(&str, &str)]) -> String {
    let mut form = format!(
        "name=renamed&wrerr_child-TOTAL_FORMS={}&wrerr_child-INITIAL_FORMS=0",
        rows.len()
    );
    for (i, (qty, note)) in rows.iter().enumerate() {
        form.push_str(&format!(
            "&wrerr_child-{i}-qty={qty}&wrerr_child-{i}-note={note}"
        ));
    }
    form
}

/// An unparsable inline value refuses the POST, parent included (#2339).
async fn bad_inline_value_rerenders_the_form(pool: &Pool) {
    let p = seed_parent(pool, "p").await;
    let form = inline_form(&[("1", "ok"), ("abc", "bad")]);
    let (status, body) = post(pool, &format!("/wrerr_parent/{p}"), &form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Child row 2"), "{body}");
    assert!(notes(pool).await.is_empty(), "inline rows saved");
    assert_eq!(parent_names(pool).await, ["p"]);
}

async fn parent_names(pool: &Pool) -> Vec<String> {
    Parent::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .map(|p| p.name)
        .collect()
}

/// A refused inline write rolls back the inline rows and the parent edit (#2339).
async fn refused_inline_write_rolls_back(pool: &Pool) {
    let p = seed_parent(pool, "p").await;
    seed_child(pool, p, "taken").await;
    let form = inline_form(&[("1", "fresh"), ("2", "taken")]);
    let (status, body) = post(pool, &format!("/wrerr_parent/{p}"), &form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(notes(pool).await, ["taken"], "partial inline write kept");
    assert_eq!(parent_names(pool).await, ["p"], "parent edit kept");
    assert_no_driver_text(&body);
    assert!(body.contains("Nothing was saved"), "{body}");
    assert!(body.contains("already exists"), "{body}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        unknown_action_is_a_400,
        unique_refusal_is_a_plain_message,
        unique_refusal_on_a_typed_pk_names_no_field,
        fk_refusal_is_a_plain_message,
        deleting_a_referenced_row_is_a_409,
        bad_inline_value_rerenders_the_form,
        refused_inline_write_rolls_back,
    ],
}
