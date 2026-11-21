//! Template views run the object-permission hooks and queryset scopes a
//! ViewSet runs, on every backend: hidden is a 404, refused a 403.

#![cfg(all(feature = "template_views", feature = "admin"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::{Filter, Model as _, Op, SqlValue};
use rustango::sql::{Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::template_views::{CreateView, DeleteView, DetailView, ListView, UpdateView};
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

/// A UUID key, seeded as upper-case text where the column keeps text.
#[derive(Model, Debug, Clone)]
#[rustango(table = "tvop_tag", app = "tvop")]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
    #[rustango(max_length = 32)]
    pub title: String,
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
fn add(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    !title(row).starts_with("noadd")
}
rustango::register_object_permission!("tvop_note", "add", add);
rustango::register_object_permission!("tvop_tag", "view", view);

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
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
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
                CreateView::for_model(schema, fields)
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
            .router("/pins", t.clone(), pool.clone()),
    )
    .merge(
        CreateView::for_model(Pin::SCHEMA, &["note"])
            .allow_anonymous()
            .template("page.html")
            .success_url("/done")
            .router("/pinform", t.clone(), pool.clone()),
    )
    .merge(
        UpdateView::for_model(Pin::SCHEMA, &["note"])
            .allow_anonymous()
            .template("page.html")
            .success_url("/done")
            .router("/pinform", t.clone(), pool.clone()),
    )
    .merge(
        ListView::for_model(Note::SCHEMA)
            .allow_anonymous()
            .template("list.html")
            .view_hook_scan_cap(2)
            .filter_fields(&["title"])
            .router("/capped", t.clone(), pool.clone()),
    )
    .merge(
        ListView::for_model(Tag::SCHEMA)
            .allow_anonymous()
            .template("list.html")
            .bulk_actions(true)
            .router("/tags", t, pool.clone()),
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

/// Past its scan cap a list is a 400 that names the knob; under it, it serves.
async fn a_list_past_its_scan_cap_is_refused(pool: &Pool) {
    notes(pool, &["a", "b", "hidden-c"]).await;
    let app = app(pool);
    let (status, body) = send(&app, Method::GET, "/capped", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("view_hook_scan_cap"), "{body}");
    let (status, body) = send(&app, Method::GET, "/capped?title=a", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(&app, Method::GET, "/notes", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.starts_with("2:"), "{body}");
}

/// A form may not point an FK at a row the target's "view" hook hides: a form error.
async fn a_form_fk_to_a_hidden_target_is_refused(pool: &Pool) {
    let ids = notes(pool, &["hidden-a", "open-b"]).await;
    let app = app(pool);
    let (status, body) = send(
        &app,
        Method::POST,
        "/pinform/new",
        &format!("note={}", ids[0]),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = send(
        &app,
        Method::POST,
        "/pinform/new",
        &format!("note={}", ids[1]),
    )
    .await;
    assert!(status.is_redirection(), "{status}");
    let pin = Pin::objects().fetch(pool).await.expect("fetch").remove(0);
    let uri = format!("/pinform/{}/edit", pin.id.get().unwrap());
    let (status, _) = send(&app, Method::POST, &uri, &format!("note={}", ids[0])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let pins = Pin::objects().fetch(pool).await.expect("fetch");
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].note.pk(), ids[1], "a refused FK was written");
}

/// A hooked bulk action matches a UUID key stored as other text.
async fn a_hooked_bulk_action_matches_uuid_keys(pool: &Pool) {
    const OPEN: &str = "0F8FAD5B-D9CB-469F-A165-70867728950E";
    const HIDDEN: &str = "7C9E6679-7425-40DE-944B-E07FC1F90AE7";
    for (id, title) in [(OPEN, "open-u"), (HIDDEN, "hidden-u")] {
        let sql = format!("INSERT INTO tvop_tag (id, title) VALUES ('{id}', '{title}')");
        rustango::sql::raw_execute_pool(pool, &sql, Vec::new())
            .await
            .expect("seed tag");
    }
    let app = app(pool);
    let picks = format!("action=delete_selected&_selected_action={OPEN}&_selected_action={HIDDEN}");
    let (status, body) = send(&app, Method::POST, "/tags", &picks).await;
    assert!(status.is_redirection(), "{status} {body}");
    // Read as text: SQLite keeps the seeded key as TEXT, which a typed fetch rejects.
    let left: Vec<String> = Tag::objects()
        .values_list_flat("title")
        .fetch::<String>(pool)
        .await
        .expect("fetch");
    assert_eq!(left, ["hidden-u"], "the visible row was not deleted");
}

/// A create runs the "add" hook, and may not write a row the "view" hook or
/// a queryset scope would hide: 403 and no row.
async fn a_create_runs_the_add_hook(pool: &Pool) {
    let app = app(pool);
    for title in ["noadd-a", "hidden-b"] {
        let (status, body) =
            send(&app, Method::POST, "/notes/new", &format!("title={title}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{title}: {body}");
    }
    let (status, _) = send(&app, Method::POST, "/owned/new", "owner_id=2").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(titles(pool).await.is_empty(), "a refused create wrote");
    assert!(Owned::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .is_empty());
    let (status, _) = send(&app, Method::POST, "/notes/new", "title=open-c").await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(titles(pool).await, ["open-c"]);
}

/// An edit that would move a row out of view or out of a queryset scope is
/// refused and rolled back.
async fn an_edit_cannot_hide_its_row(pool: &Pool) {
    let id = notes(pool, &["open-a"]).await[0];
    let mut own = Owned {
        id: Auto::default(),
        owner_id: 1,
    };
    own.insert(pool).await.expect("seed");
    let app = app(pool);
    let uri = format!("/notes/{id}/edit");
    let (status, _) = send(&app, Method::POST, &uri, "title=hidden-now").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let uri = format!("/owned/{}/edit", own.id.get().unwrap());
    let (status, _) = send(&app, Method::POST, &uri, "owner_id=2").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(titles(pool).await, ["open-a"], "a refused edit wrote");
    let left = Owned::objects().fetch(pool).await.expect("fetch");
    assert_eq!(left[0].owner_id, 1, "a refused edit wrote");
}

/// The hooks judge the row locked in the write's transaction: an edit or
/// delete that waits on a concurrent change sees it. SQLite serializes writers.
async fn a_concurrent_change_is_judged_after_it_lands(pool: &Pool) {
    use rustango::core::{Assignment, UpdateQuery, WhereExpr};
    if pool.dialect().kind() == rustango::sql::DialectKind::Sqlite {
        return;
    }
    let ids = notes(pool, &["open-1", "open-2"]).await;
    let app = app(pool);
    for (id, title, action, form) in [
        (ids[0], "locked-1", "edit", "title=open-x"),
        (ids[1], "keep-2", "delete", ""),
    ] {
        let mut tx = rustango::sql::write_transaction(pool).await.expect("tx");
        let edit = UpdateQuery::new(
            Note::SCHEMA,
            vec![Assignment::new("title", SqlValue::from(title))],
            WhereExpr::Predicate(Filter::new("id", Op::Eq, SqlValue::I64(id))),
        );
        rustango::sql::update_tx(&mut tx, &edit)
            .await
            .expect("edit");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let call = tokio::spawn({
            let (app, barrier) = (app.clone(), barrier.clone());
            let uri = format!("/notes/{id}/{action}");
            async move {
                barrier.wait().await;
                send(&app, Method::POST, &uri, form).await.0
            }
        });
        barrier.wait().await;
        // Long enough for the request to block on the row lock.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        tx.commit().await.expect("commit");
        assert_eq!(call.await.unwrap(), StatusCode::FORBIDDEN, "{action}");
    }
    assert_eq!(titles(pool).await, ["keep-2", "locked-1"]);
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        a_create_runs_the_add_hook,
        an_edit_cannot_hide_its_row,
        a_concurrent_change_is_judged_after_it_lands,
        a_list_past_its_scan_cap_is_refused,
        a_form_fk_to_a_hidden_target_is_refused,
        a_hooked_bulk_action_matches_uuid_keys,
        item_views_hide_with_404_and_refuse_with_403,
        lists_and_bulk_actions_skip_hidden_rows,
        a_queryset_scope_narrows_template_views,
    ],
);
