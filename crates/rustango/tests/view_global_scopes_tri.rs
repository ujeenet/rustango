//! `ViewSet` and the template views apply the model's global scopes:
//! a row every `QuerySet` hides is not listed, counted, read or written (#1746).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    any(feature = "admin", feature = "tenancy"),
    feature = "template_views"
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::{Filter, Model as _, Op, SqlValue, WhereExpr};
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, ForeignKey, Pool, UpdaterPool as _};
use rustango::template_views::BulkActionFn;
use rustango::template_views::{DeleteView, DetailView, ListView, UpdateView};
use rustango::viewset::ViewSet;
use rustango::{tri_dialect_test, Model};
use tera::Tera;
use tower::ServiceExt as _;

fn visible_only() -> WhereExpr {
    WhereExpr::Predicate(Filter::new("visible", Op::Eq, SqlValue::Bool(true)))
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "scope1746_note",
    app = "scope1746",
    global_scope(name = "visible", apply = visible_only)
)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub tag: String,
    pub visible: bool,
}

/// A top-level OR, so an appended search clause must not bind into it.
fn listed_only() -> WhereExpr {
    WhereExpr::Or(vec![
        WhereExpr::Predicate(Filter::new("visible", Op::Eq, SqlValue::Bool(true))),
        WhereExpr::Predicate(Filter::new("name", Op::Eq, SqlValue::from("keep"))),
    ])
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "scope1746_owner",
    app = "scope1746",
    display = "name",
    global_scope(name = "listed", apply = listed_only)
)]
pub struct Owner {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub visible: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "scope1746_pet", app = "scope1746")]
pub struct Pet {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub tag: String,
    pub owner_id: ForeignKey<Owner, i64>,
}

const CSRF: &str = "scope1746-csrf-token-scope1746-csrf-token-x";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Pet::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Owner>(pool).await;
    rustango::testkit::matrix::fresh_table::<Pet>(pool).await;
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
}

/// A custom bulk action that writes unscoped, so only the PK narrowing
/// keeps a hidden row out of it.
fn retag_action() -> BulkActionFn {
    Arc::new(|pool, pks| {
        Box::pin(async move {
            Note::objects()
                .without_global_scopes()
                .filter("id__in", SqlValue::List(pks.to_vec()))
                .update()
                .set("tag", "m")
                .execute_pool(pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    })
}

/// Seeds three visible rows and two hidden ones; returns `(visible, hidden)` PKs.
async fn seed(pool: &Pool) -> (Vec<i64>, Vec<i64>) {
    let (mut shown, mut hidden) = (Vec::new(), Vec::new());
    for (tag, visible) in [
        ("a", true),
        ("h", false),
        ("b", true),
        ("h", false),
        ("c", true),
    ] {
        let mut row = Note {
            id: Auto::default(),
            tag: tag.into(),
            visible,
        };
        row.insert_pool(pool).await.expect("seed row");
        let pk = *row.id.get().expect("pk");
        if visible {
            shown.push(pk)
        } else {
            hidden.push(pk)
        }
    }
    (shown, hidden)
}

async fn all_rows(pool: &Pool) -> Vec<Note> {
    Note::objects()
        .without_global_scopes()
        .fetch(pool)
        .await
        .expect("unscoped fetch")
}

fn tera() -> Arc<Tera> {
    let mut t = Tera::default();
    t.add_raw_templates(vec![
        (
            "list.html",
            "rows={{ object_list | length }} total={{ total }}",
        ),
        ("detail.html", "tag={{ object.tag }}"),
        ("form.html", "form"),
        ("confirm.html", "confirm"),
        ("bulk.html", "objects={{ objects | length }}"),
        (
            "pets.html",
            r#"{% for r in object_list %}{{ r.tag }}:{{ r.owner_id_display | default(value="-") }};{% endfor %}"#,
        ),
    ])
    .unwrap();
    Arc::new(t)
}

fn app(pool: &Pool) -> axum::Router {
    let t = tera();
    ViewSet::for_model(Note::SCHEMA)
        .filter_fields(&["tag"])
        .search_fields(&["tag"])
        .router_pool("/api/notes", pool.clone())
        .merge(
            ViewSet::for_model(Note::SCHEMA)
                .cursor_pagination("id")
                .router_pool("/api/cursor", pool.clone()),
        )
        .merge(
            ListView::for_model(Note::SCHEMA)
                .template("list.html")
                .filter_fields(&["tag"])
                .bulk_actions(true)
                .action("retag", "Retag", retag_action())
                .with_delete_confirmation(true)
                .with_delete_confirmation_template("bulk.html")
                .router("/notes", t.clone(), pool.clone()),
        )
        .merge(
            ListView::for_model(Pet::SCHEMA)
                .template("pets.html")
                .with_fk_display(true)
                .router("/pets", t.clone(), pool.clone()),
        )
        .merge(
            ViewSet::for_model(Owner::SCHEMA)
                .search_fields(&["name"])
                .router_pool("/api/owners", pool.clone()),
        )
        .merge(
            DetailView::for_model(Note::SCHEMA)
                .template("detail.html")
                .router("/notes", t.clone(), pool.clone()),
        )
        .merge(
            UpdateView::for_model(Note::SCHEMA)
                .template("form.html")
                .success_url("/notes")
                .router("/notes", t.clone(), pool.clone()),
        )
        .merge(
            DeleteView::for_model(Note::SCHEMA)
                .template("confirm.html")
                .success_url("/notes")
                .router("/notes", t, pool.clone()),
        )
}

async fn send(
    pool: &Pool,
    method: Method,
    uri: &str,
    ctype: &str,
    body: &str,
) -> (StatusCode, String) {
    let body = if ctype.starts_with("application/x-www-form-urlencoded") {
        format!("_csrf={CSRF}&{body}")
    } else {
        body.to_owned()
    };
    let resp = app(pool)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, ctype)
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(pool: &Pool, uri: &str) -> (StatusCode, String) {
    send(pool, Method::GET, uri, "text/plain", "").await
}

const JSON: &str = "application/json";
const FORM: &str = "application/x-www-form-urlencoded";

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

async fn viewset_list_hides_scoped_rows(pool: &Pool) {
    seed(pool).await;
    let (status, body) = get(pool, "/api/notes").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v = json(&body);
    assert_eq!(v["count"], 3, "{body}");
    assert_eq!(v["results"].as_array().unwrap().len(), 3, "{body}");

    for uri in ["/api/notes?tag=h", "/api/notes?search=h"] {
        let (_, body) = get(pool, uri).await;
        assert_eq!(json(&body)["count"], 0, "{uri}: {body}");
    }

    let (_, body) = get(pool, "/api/cursor").await;
    assert_eq!(
        json(&body)["results"].as_array().unwrap().len(),
        3,
        "{body}"
    );
}

async fn viewset_pk_actions_404_on_hidden_rows(pool: &Pool) {
    let (shown, hidden) = seed(pool).await;
    let h = hidden[0];
    let (status, _) = get(pool, &format!("/api/notes/{}", shown[0])).await;
    assert_eq!(status, StatusCode::OK, "control: a visible row reads");

    let (status, _) = get(pool, &format!("/api/notes/{h}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "retrieve");
    let put = r#"{"tag":"x","visible":true}"#;
    let (status, _) = send(pool, Method::PUT, &format!("/api/notes/{h}"), JSON, put).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "update");
    let (status, _) = send(
        pool,
        Method::PATCH,
        &format!("/api/notes/{h}"),
        JSON,
        r#"{"tag":"x"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "partial update");
    let (status, _) = send(pool, Method::DELETE, &format!("/api/notes/{h}"), JSON, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "destroy");

    let rows = all_rows(pool).await;
    assert_eq!(rows.len(), 5, "nothing deleted");
    assert!(
        rows.iter().filter(|r| !r.visible).all(|r| r.tag == "h"),
        "nothing updated"
    );
}

async fn list_view_hides_scoped_rows(pool: &Pool) {
    seed(pool).await;
    let (status, body) = get(pool, "/notes").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "rows=3 total=3");
    let (_, body) = get(pool, "/notes?tag=h").await;
    assert_eq!(body, "rows=0 total=0");
}

async fn detail_update_delete_views_404_on_hidden_rows(pool: &Pool) {
    let (shown, hidden) = seed(pool).await;
    let h = hidden[0];
    let (status, _) = get(pool, &format!("/notes/{}", shown[0])).await;
    assert_eq!(status, StatusCode::OK, "control: a visible row renders");

    for uri in [
        format!("/notes/{h}"),
        format!("/notes/{h}/edit"),
        format!("/notes/{h}/delete"),
    ] {
        let (status, _) = get(pool, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}");
    }
    let (status, _) = send(
        pool,
        Method::POST,
        &format!("/notes/{h}/edit"),
        FORM,
        "tag=x&visible=true",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "UpdateView POST");
    let (status, _) = send(pool, Method::POST, &format!("/notes/{h}/delete"), FORM, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "DeleteView POST");

    let rows = all_rows(pool).await;
    assert_eq!(rows.len(), 5, "nothing deleted");
    assert!(
        rows.iter().filter(|r| !r.visible).all(|r| r.tag == "h"),
        "nothing updated"
    );
}

async fn bulk_delete_skips_hidden_rows(pool: &Pool) {
    let (shown, hidden) = seed(pool).await;
    let pick = format!(
        "action=delete_selected&_selected_action={}&_selected_action={}",
        hidden[0], shown[0]
    );
    let (status, body) = send(pool, Method::POST, "/notes", FORM, &pick).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body, "objects=1",
        "the confirm page lists only the visible row"
    );

    let (status, body) = send(
        pool,
        Method::POST,
        "/notes",
        FORM,
        &format!("{pick}&confirmed=true"),
    )
    .await;
    assert!(status.is_redirection(), "{status}: {body}");
    let rows = all_rows(pool).await;
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter().any(|r| *r.id.get().unwrap() == hidden[0]),
        "hidden row kept"
    );
    assert_eq!(Note::objects().count(pool).await.unwrap(), 2);
}

async fn note(pool: &Pool, pk: i64) -> Option<Note> {
    all_rows(pool)
        .await
        .into_iter()
        .find(|r| *r.id.get().unwrap() == pk)
}

async fn viewset_writes_on_visible_rows_work(pool: &Pool) {
    let (shown, _) = seed(pool).await;
    let (status, body) = send(
        pool,
        Method::PATCH,
        &format!("/api/notes/{}", shown[0]),
        JSON,
        r#"{"tag":"p"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PATCH: {body}");
    assert_eq!(json(&body)["tag"], "p");
    let put = r#"{"tag":"u","visible":true}"#;
    let uri = format!("/api/notes/{}", shown[1]);
    let (status, body) = send(pool, Method::PUT, &uri, JSON, put).await;
    assert_eq!(status, StatusCode::OK, "PUT: {body}");
    assert_eq!(json(&body)["tag"], "u");
    let uri = format!("/api/notes/{}", shown[2]);
    let (status, _) = send(pool, Method::DELETE, &uri, JSON, "").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "DELETE");

    assert_eq!(note(pool, shown[0]).await.unwrap().tag, "p");
    assert_eq!(note(pool, shown[1]).await.unwrap().tag, "u");
    assert!(note(pool, shown[2]).await.is_none(), "deleted");
}

async fn viewset_update_out_of_scope_is_204(pool: &Pool) {
    let (shown, _) = seed(pool).await;
    let uri = format!("/api/notes/{}", shown[0]);
    let (status, body) = send(pool, Method::PATCH, &uri, JSON, r#"{"visible":false}"#).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "PATCH: {body}");
    assert!(body.is_empty(), "no row echoed: {body}");
    let uri = format!("/api/notes/{}", shown[1]);
    let put = r#"{"tag":"q","visible":false}"#;
    let (status, body) = send(pool, Method::PUT, &uri, JSON, put).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "PUT: {body}");

    assert!(
        !note(pool, shown[0]).await.unwrap().visible,
        "PATCH committed"
    );
    let moved = note(pool, shown[1]).await.unwrap();
    assert!(!moved.visible && moved.tag == "q", "PUT committed");
}

async fn viewset_create_out_of_scope_is_201_without_body(pool: &Pool) {
    let (status, body) = send(
        pool,
        Method::POST,
        "/api/notes",
        JSON,
        r#"{"tag":"n","visible":false}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body.is_empty(), "no row echoed: {body}");

    let (status, body) = send(
        pool,
        Method::POST,
        "/api/notes",
        JSON,
        r#"{"tag":"v","visible":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(json(&body)["tag"], "v");

    let bulk = r#"[{"tag":"bv","visible":true},{"tag":"bh","visible":false}]"#;
    let (status, body) = send(pool, Method::POST, "/api/notes", JSON, bulk).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let v = json(&body);
    assert_eq!(v[0]["tag"], "bv", "{body}");
    assert!(v[1].is_null(), "hidden entry not echoed: {body}");

    let tags: Vec<String> = all_rows(pool).await.into_iter().map(|r| r.tag).collect();
    assert_eq!(tags.len(), 4, "every row written: {tags:?}");
}

async fn custom_bulk_action_gets_only_visible_pks(pool: &Pool) {
    let (shown, hidden) = seed(pool).await;
    let pick = format!(
        "action=retag&_selected_action={}&_selected_action={}",
        hidden[0], shown[0]
    );
    let (status, body) = send(pool, Method::POST, "/notes", FORM, &pick).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(note(pool, shown[0]).await.unwrap().tag, "m", "visible row");
    assert_eq!(note(pool, hidden[0]).await.unwrap().tag, "h", "hidden row");
}

async fn template_writes_on_visible_rows_work(pool: &Pool) {
    let (shown, _) = seed(pool).await;
    let (status, body) = send(
        pool,
        Method::POST,
        &format!("/notes/{}/edit", shown[0]),
        FORM,
        "tag=e&visible=true",
    )
    .await;
    assert!(status.is_redirection(), "UpdateView: {status}: {body}");
    let uri = format!("/notes/{}/delete", shown[1]);
    let (status, body) = send(pool, Method::POST, &uri, FORM, "").await;
    assert!(status.is_redirection(), "DeleteView: {status}: {body}");

    assert_eq!(note(pool, shown[0]).await.unwrap().tag, "e");
    assert!(note(pool, shown[1]).await.is_none(), "deleted");
}

/// Seeds `ann` (visible), `bob` (hidden) and `keep` (listed by name).
async fn seed_owners(pool: &Pool) -> Vec<i64> {
    let mut pks = Vec::new();
    for (name, visible) in [("ann", true), ("bob", false), ("keep", false)] {
        let mut o = Owner {
            id: Auto::default(),
            name: name.into(),
            visible,
        };
        o.insert_pool(pool).await.expect("seed owner");
        pks.push(*o.id.get().expect("pk"));
    }
    pks
}

async fn fk_display_skips_scoped_out_targets(pool: &Pool) {
    let owners = seed_owners(pool).await;
    for (tag, owner) in [("p1", owners[0]), ("p2", owners[1])] {
        let mut p = Pet {
            id: Auto::default(),
            tag: tag.into(),
            owner_id: ForeignKey::unloaded(owner),
        };
        p.insert_pool(pool).await.expect("seed pet");
    }
    let (status, body) = get(pool, "/pets").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "p1:ann;p2:-;");
}

async fn search_narrows_an_or_scope(pool: &Pool) {
    seed_owners(pool).await;
    let (_, body) = get(pool, "/api/owners").await;
    assert_eq!(json(&body)["count"], 2, "{body}");
    for (q, n) in [("ann", 1), ("keep", 1), ("bob", 0)] {
        let (_, body) = get(pool, &format!("/api/owners?search={q}")).await;
        let v = json(&body);
        assert_eq!(v["count"], n, "?search={q}: {body}");
        assert_eq!(v["results"].as_array().unwrap().len(), n, "{body}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        viewset_list_hides_scoped_rows,
        viewset_pk_actions_404_on_hidden_rows,
        list_view_hides_scoped_rows,
        detail_update_delete_views_404_on_hidden_rows,
        bulk_delete_skips_hidden_rows,
        viewset_writes_on_visible_rows_work,
        viewset_update_out_of_scope_is_204,
        viewset_create_out_of_scope_is_201_without_body,
        custom_bulk_action_gets_only_visible_pks,
        template_writes_on_visible_rows_work,
        fk_display_skips_scoped_out_targets,
        search_narrows_an_or_scope,
    ],
}
