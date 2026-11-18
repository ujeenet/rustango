//! Template views run the object-permission hooks and queryset scopes a
//! ViewSet runs, on every backend: hidden is a 404, refused a 403.

#![cfg(all(feature = "template_views", feature = "admin"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::{Filter, Model as _, Op, SqlValue};
use rustango::sql::{Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::template_views::{DeleteView, DetailView, ListView, UpdateView};
use rustango::{tri_dialect_test, Model};
use serde_json::Value;
use tera::Tera;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "tvop_note", app = "tvop", display = "title")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tvop_owned", app = "tvop")]
pub struct Owned {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub owner_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tvop_pin", app = "tvop")]
pub struct Pin {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub note: ForeignKey<Note>,
}

fn title(row: Option<&Value>) -> &str {
    row.and_then(|r| r["title"].as_str()).unwrap_or("")
}
fn view(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    !title(row).starts_with("hidden")
}
fn change(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    !title(row).starts_with("locked")
}
fn delete(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    !title(row).starts_with("keep")
}
rustango::register_object_permission!("tvop_note", "view", view);
rustango::register_object_permission!("tvop_note", "change", change);
rustango::register_object_permission!("tvop_note", "delete", delete);

fn owner_one(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("owner_id", Op::Eq, SqlValue::I64(1))]
}
rustango::register_queryset_scope!("tvop_owned", owner_one);

const CSRF: &str = "tvop-csrf-token-tvop-csrf-token-tvop-csrf-x";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Pin::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    rustango::testkit::matrix::fresh_table::<Pin>(pool).await;
    rustango::testkit::matrix::fresh_table::<Owned>(pool).await;
}

fn app(pool: &Pool) -> axum::Router {
    let mut t = Tera::default();
    t.add_raw_templates(vec![
        ("page.html", "ok"),
        (
            "list.html",
            "{{ total }}:{% for r in object_list %}{{ r.title | default(value=r.id) }};{% endfor %}",
        ),
        (
            "pins.html",
            r#"{% for r in object_list %}{{ r.note_display | default(value="-") }};{% endfor %}"#,
        ),
    ])
    .unwrap();
    let t = Arc::new(t);
    let mut app = axum::Router::new();
    for (prefix, schema) in [("/notes", Note::SCHEMA), ("/owned", Owned::SCHEMA)] {
        let fields: &[&str] = if prefix == "/notes" {
            &["title"]
        } else {
            &["owner_id"]
        };
        app = app
            .merge(
                ListView::for_model(schema)
                    .allow_anonymous()
                    .template("list.html")
                    .bulk_actions(true)
                    .router(prefix, t.clone(), pool.clone()),
            )
            .merge(
                DetailView::for_model(schema)
                    .allow_anonymous()
                    .template("page.html")
                    .router(prefix, t.clone(), pool.clone()),
            )
            .merge(
                UpdateView::for_model(schema, fields)
                    .allow_anonymous()
                    .template("page.html")
                    .success_url("/done")
                    .router(prefix, t.clone(), pool.clone()),
            )
            .merge(
                DeleteView::for_model(schema)
                    .allow_anonymous()
                    .template("page.html")
                    .success_url("/done")
                    .router(prefix, t.clone(), pool.clone()),
            );
    }
    app.merge(
        ListView::for_model(Pin::SCHEMA)
            .allow_anonymous()
            .template("pins.html")
            .with_fk_display(true)
            .router("/pins", t, pool.clone()),
    )
}

async fn send(app: &axum::Router, method: Method, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
        .body(Body::from(format!("_csrf={CSRF}&{body}")))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn notes(pool: &Pool, titles: &[&str]) -> Vec<i64> {
    let mut ids = Vec::new();
    for t in titles {
        let mut n = Note {
            id: Auto::default(),
            title: (*t).to_owned(),
        };
        n.insert(pool).await.expect("seed note");
        ids.push(*n.id.get().expect("pk"));
    }
    ids
}

async fn titles(pool: &Pool) -> Vec<String> {
    let mut t: Vec<String> = Note::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .map(|n| n.title)
        .collect();
    t.sort();
    t
}

/// Hidden reads as absent on every item view; a visible row a hook refuses is a 403.
async fn item_views_hide_with_404_and_refuse_with_403(pool: &Pool) {
    let ids = notes(pool, &["hidden-a", "locked-b", "keep-c", "open-d"]).await;
    let (hidden, locked, keep, open) = (ids[0], ids[1], ids[2], ids[3]);
    let app = app(pool);
    let get = |uri: String| {
        let app = app.clone();
        async move { send(&app, Method::GET, &uri, "").await.0 }
    };
    assert_eq!(get(format!("/notes/{hidden}")).await, StatusCode::NOT_FOUND);
    assert_eq!(get(format!("/notes/{open}")).await, StatusCode::OK);
    assert_eq!(
        get(format!("/notes/{hidden}/edit")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(format!("/notes/{locked}/edit")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(format!("/notes/{hidden}/delete")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(format!("/notes/{keep}/delete")).await,
        StatusCode::FORBIDDEN
    );
    let post = |uri: String, body: &'static str| {
        let app = app.clone();
        async move { send(&app, Method::POST, &uri, body).await.0 }
    };
    assert_eq!(
        post(format!("/notes/{hidden}/edit"), "title=x").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(format!("/notes/{locked}/edit"), "title=x").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(format!("/notes/{hidden}/delete"), "").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(format!("/notes/{keep}/delete"), "").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        titles(pool).await,
        ["hidden-a", "keep-c", "locked-b", "open-d"],
        "a refused write changed a row"
    );
    assert!(post(format!("/notes/{open}/edit"), "title=open-e")
        .await
        .is_redirection());
    assert!(post(format!("/notes/{open}/delete"), "")
        .await
        .is_redirection());
    assert_eq!(titles(pool).await, ["hidden-a", "keep-c", "locked-b"]);
}

/// A list leaves hidden rows out of its rows and its total; a bulk delete
/// skips them and refuses a row its "delete" hook refuses.
async fn lists_and_bulk_actions_skip_hidden_rows(pool: &Pool) {
    let ids = notes(pool, &["hidden-a", "keep-b", "open-c", "open-d"]).await;
    let app = app(pool);
    let (status, body) = send(&app, Method::GET, "/notes", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.starts_with("3:"), "{body}");
    assert!(!body.contains("hidden"), "{body}");
    let bulk = |pks: &[i64]| {
        let picks: String = pks
            .iter()
            .map(|p| format!("&_selected_action={p}"))
            .collect();
        format!("action=delete_selected{picks}")
    };
    let (status, _) = send(&app, Method::POST, "/notes", &bulk(&[ids[1], ids[2]])).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&app, Method::POST, "/notes", &bulk(&[ids[0], ids[3]])).await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(titles(pool).await, ["hidden-a", "keep-b", "open-c"]);
    // An FK to a hidden row shows no display name.
    for id in [ids[0], ids[2]] {
        let mut p = Pin {
            id: Auto::default(),
            note: ForeignKey::unloaded(id),
        };
        p.insert(pool).await.expect("seed pin");
    }
    let (_, body) = send(&app, Method::GET, "/pins", "").await;
    assert_eq!(body, "-;open-c;");
}

/// A queryset scope narrows every template view.
async fn a_queryset_scope_narrows_template_views(pool: &Pool) {
    let mut ids = Vec::new();
    for owner in [1, 2] {
        let mut o = Owned {
            id: Auto::default(),
            owner_id: owner,
        };
        o.insert(pool).await.expect("seed");
        ids.push(*o.id.get().expect("pk"));
    }
    let other = ids[1];
    let app = app(pool);
    let (_, body) = send(&app, Method::GET, "/owned", "").await;
    assert!(body.starts_with("1:"), "{body}");
    for (method, uri, form) in [
        (Method::GET, format!("/owned/{other}"), ""),
        (Method::GET, format!("/owned/{other}/edit"), ""),
        (Method::POST, format!("/owned/{other}/edit"), "owner_id=1"),
        (Method::POST, format!("/owned/{other}/delete"), ""),
    ] {
        let (status, _) = send(&app, method.clone(), &uri, form).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    let picks = format!("action=delete_selected&_selected_action={other}");
    let (status, _) = send(&app, Method::POST, "/owned", &picks).await;
    assert!(status.is_redirection(), "{status}");
    let left = Owned::objects().fetch(pool).await.expect("fetch");
    assert_eq!(left.len(), 2, "a scoped-out row was written");
    assert!(left.iter().any(|o| o.owner_id == 2));
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        item_views_hide_with_404_and_refuse_with_403,
        lists_and_bulk_actions_skip_hidden_rows,
        a_queryset_scope_narrows_template_views,
    ],
);
