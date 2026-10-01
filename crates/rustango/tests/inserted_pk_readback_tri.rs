//! ModelForm and the admin report the PK the INSERT wrote (#1894), refuse
//! before writing when MySQL cannot read it back (#1978), and fill v7 PKs (#1725).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{header, Method, Request};
use rustango::core::{Model as _, SqlValue};
use rustango::forms::ModelForm;
use rustango::sql::{CounterPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1894_tag", app = "pk1894")]
pub struct Tag {
    #[rustango(primary_key, max_length = 64)]
    pub slug: String,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1894_token", app = "pk1894")]
pub struct Token {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1978_coupon", app = "pk1894")]
pub struct Coupon {
    #[rustango(
        primary_key,
        auto_uuid,
        default = "'7c9e6679-7425-40de-944b-e07fc1f90ae7'"
    )]
    pub id: rustango::sql::Auto<uuid::Uuid>,
    #[rustango(max_length = 32)]
    pub name: String,
}

/// Integer PK beside a generated `f64`, which joins RETURNING.
#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1978_line", app = "pk1894")]
pub struct Line {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub price: f64,
    #[rustango(generated_as = "price * 2")]
    pub doubled: f64,
}

/// A Rust-filled v7 PK, with an inline child that has one too (#1725).
#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1725_doc", app = "pk1894")]
pub struct V7Doc {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: rustango::sql::Auto<uuid::Uuid>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1725_line", app = "pk1894")]
pub struct V7Line {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: rustango::sql::Auto<uuid::Uuid>,
    #[rustango(fk = "pk1725_doc", on = "id")]
    pub doc_id: uuid::Uuid,
    #[rustango(max_length = 32)]
    pub title: String,
}

rustango::register_admin_inline!(
    parent = "pk1725_doc",
    child = "pk1725_line",
    fk = "doc_id",
    fields = &["doc_id", "title"],
);

const UUID_A: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const UUID_DEFAULT: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Token>(pool).await;
    rustango::testkit::matrix::fresh_table::<Coupon>(pool).await;
    rustango::testkit::matrix::fresh_table::<Line>(pool).await;
    rustango::testkit::matrix::drop_table(pool, V7Line::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<V7Doc>(pool).await;
    rustango::testkit::matrix::fresh_table::<V7Line>(pool).await;
}

fn v7(pk: &str) -> uuid::Uuid {
    let id = uuid::Uuid::parse_str(pk).unwrap_or_else(|e| panic!("{pk:?}: {e}"));
    assert_eq!(id.get_version_num(), 7, "{pk}");
    id
}

async fn model_form_returns_the_written_pk(pool: &Pool) {
    let data = HashMap::from([("name".to_owned(), "Rust".to_owned())]);
    // A natural PK is form input since #1725; a server-set one is excluded.
    let mut prep = ModelForm::new(Tag::SCHEMA, data.clone())
        .exclude(&["slug"])
        .prepare_save()
        .expect("valid");
    prep.set("slug", "rust");
    let pk = prep.commit_pool(pool).await.expect("insert tag");
    assert_eq!(pk, SqlValue::String("rust".into()));

    let uuid = uuid::Uuid::parse_str(UUID_A).unwrap();
    let mut prep = ModelForm::new(Token::SCHEMA, data)
        .exclude(&["id"])
        .prepare_save()
        .expect("valid");
    prep.set("id", uuid);
    let pk = prep.commit_pool(pool).await.expect("insert token");
    assert_eq!(pk, SqlValue::Uuid(uuid));
}

async fn location(app: axum::Router, uri: &str, body: String) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    loc.unwrap_or_else(|| panic!("{uri}: no redirect, status {status}"))
}

async fn admin_create_redirects_to_the_new_pk(pool: &Pool) {
    let admin = || {
        rustango::admin::Builder::new(pool.clone())
            .admin_prefix("")
            .build()
    };
    let got = location(
        admin(),
        "/pk1894_tag",
        "slug=rust&name=Rust&_continue=1".into(),
    )
    .await;
    assert_eq!(got, "/pk1894_tag/rust");
    let got = location(
        admin(),
        "/pk1894_token",
        format!("id={UUID_A}&name=T&_continue=1"),
    )
    .await;
    assert_eq!(got, format!("/pk1894_token/{UUID_A}"));
}

async fn post(app: axum::Router, uri: &str, body: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
}

/// A DB-default UUID PK: read back where RETURNING exists; on MySQL
/// refused before the INSERT, so a re-submit can't duplicate the row.
async fn db_default_pk_is_read_or_refused_before_the_insert(pool: &Pool) {
    let admin = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let resp = post(admin, "/pk1978_coupon", "name=C&_continue=1").await;
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let form = ModelForm::new(Coupon::SCHEMA, HashMap::from([("name".into(), "F".into())]))
        .prepare_save()
        .expect("valid");
    let committed = form.commit_pool(pool).await;
    let rows = Coupon::objects().count(pool).await.expect("count");
    if pool.dialect().name() == "mysql" {
        assert_eq!(loc, None, "MySQL cannot know the new PK");
        assert!(committed.is_err(), "{committed:?}");
        assert_eq!(rows, 0, "refused before the INSERT");
    } else {
        assert_eq!(loc, Some(format!("/pk1978_coupon/{UUID_DEFAULT}")));
        // The second INSERT hits the same default PK.
        assert!(committed.is_err(), "{committed:?}");
        assert_eq!(rows, 1);
    }
}

/// Only the PK decides the refusal: non-PK RETURNING columns still insert.
async fn integer_pk_inserts_beside_non_integer_returning_columns(pool: &Pool) {
    let mut line = Line {
        id: rustango::sql::Auto::Unset,
        price: 1.5,
        doubled: 0.0,
    };
    line.insert_pool(pool).await.expect("insert");
    assert!(
        matches!(line.id, rustango::sql::Auto::Set(_)),
        "{:?}",
        line.id
    );
    assert_eq!(Line::objects().count(pool).await.expect("count"), 1);
    // MySQL has no RETURNING, so the placeholder stays until a re-read.
    let want = if pool.dialect().name() == "mysql" {
        0.0
    } else {
        3.0
    };
    assert_eq!(line.doubled, want);
}

fn v7_admin(pool: &Pool) -> axum::Router {
    rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build()
}

async fn admin_create_fills_a_v7_pk(pool: &Pool) {
    let loc = location(v7_admin(pool), "/pk1725_doc", "name=A&_continue=1".into()).await;
    v7(loc.strip_prefix("/pk1725_doc/").expect("detail url"));
}

async fn model_form_fills_a_v7_pk(pool: &Pool) {
    let form = ModelForm::new(V7Doc::SCHEMA, HashMap::from([("name".into(), "B".into())]))
        .prepare_save()
        .expect("valid");
    match form.commit_pool(pool).await.expect("model form insert") {
        SqlValue::Uuid(id) => assert_eq!(id.get_version_num(), 7, "{id}"),
        other => panic!("ModelForm pk: {other:?}"),
    }
}

async fn inline_row_fills_a_v7_pk(pool: &Pool) {
    let mut doc = V7Doc {
        id: rustango::sql::Auto::Unset,
        name: "A".into(),
    };
    doc.insert_pool(pool).await.expect("parent");
    let doc = *doc.id.get().expect("parent pk");
    let body = format!(
        "name=A&pk1725_line-TOTAL_FORMS=1&pk1725_line-INITIAL_FORMS=0\
         &pk1725_line-MAX_NUM_FORMS=&pk1725_line-0-doc_id={doc}&pk1725_line-0-title=L"
    );
    let resp = post(v7_admin(pool), &format!("/pk1725_doc/{doc}"), &body).await;
    assert!(resp.status().is_redirection(), "inline: {}", resp.status());
    let lines: Vec<V7Line> = rustango::sql::FetcherPool::fetch(V7Line::objects(), pool)
        .await
        .expect("lines");
    assert_eq!(lines.len(), 1, "inline row written");
    v7(&lines[0].id.get().expect("pk").to_string());
    assert_eq!(lines[0].doc_id, doc);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        model_form_returns_the_written_pk,
        admin_create_redirects_to_the_new_pk,
        db_default_pk_is_read_or_refused_before_the_insert,
        integer_pk_inserts_beside_non_integer_returning_columns,
        admin_create_fills_a_v7_pk,
        model_form_fills_a_v7_pk,
        inline_row_fills_a_v7_pk,
    ],
}
