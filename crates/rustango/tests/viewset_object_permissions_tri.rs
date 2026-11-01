//! A ViewSet runs the object-permission hooks and queryset scopes the
//! admin runs, on every backend: hidden is a 404, refused a 403.

#![cfg(all(feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::core::{Filter, Model as _, Op, SqlValue};
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::tenancy::middleware::AuthenticatedUser;
use rustango::tenancy::{Principal, PrincipalKind, User};
use rustango::tri_dialect_test;
use rustango::viewset::ViewSet;
use serde_json::{json, Value};
use tower::ServiceExt as _;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_soft")]
#[allow(dead_code)]
pub struct Soft {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_scoped")]
#[allow(dead_code)]
pub struct Scoped {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub owner_id: i64,
    #[rustango(fk = "vop_label", on = "id")]
    pub label_id: Option<i64>,
}

/// A row the "view" hook admits once, then hides: a change between a
/// list's scan and its render.
#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_flip")]
#[allow(dead_code)]
pub struct Flip {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_legacy")]
#[allow(dead_code)]
pub struct Legacy {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_label")]
#[allow(dead_code)]
pub struct Label {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "vop_card")]
#[allow(dead_code)]
pub struct Card {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "vop_label", on = "id")]
    pub label_id: i64,
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
rustango::register_object_permission!("vop_doc", "view", view);
rustango::register_object_permission!("vop_doc", "change", change);
rustango::register_object_permission!("vop_doc", "delete", delete);
rustango::register_object_permission!("vop_soft", "delete", delete);

fn owner_one(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("owner_id", Op::Eq, SqlValue::I64(1))]
}
rustango::register_queryset_scope!("vop_scoped", owner_one);

static FLIPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn flip(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    use std::sync::atomic::Ordering;
    title(row) != "flip" || !FLIPPED.swap(true, Ordering::SeqCst)
}
rustango::register_object_permission!("vop_flip", "view", flip);

/// The old macro names still register, with a deprecation warning.
mod legacy {
    #![allow(deprecated)]
    use super::*;
    fn not_hidden(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
        !title(row).starts_with("hidden")
    }
    fn not_out(_: &axum::http::request::Parts) -> Vec<Filter> {
        vec![Filter::new("title", Op::Ne, SqlValue::from("out"))]
    }
    rustango::register_admin_object_permission!("vop_legacy", "view", not_hidden);
    rustango::register_admin_queryset!("vop_legacy", not_out);
}

fn not_secret(_: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    row.is_none_or(|r| r["name"] != "secret")
}
rustango::register_object_permission!("vop_label", "view", not_secret);

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
    rustango::testkit::matrix::fresh_table::<Soft>(pool).await;
    rustango::testkit::matrix::fresh_table::<Flip>(pool).await;
    rustango::testkit::matrix::fresh_table::<Legacy>(pool).await;
    // The FK owners go first so their target can be dropped.
    rustango::testkit::matrix::drop_table(pool, "vop_card").await;
    rustango::testkit::matrix::drop_table(pool, "vop_scoped").await;
    rustango::testkit::matrix::fresh_table::<Label>(pool).await;
    rustango::testkit::matrix::fresh_table::<Card>(pool).await;
    rustango::testkit::matrix::fresh_table::<Scoped>(pool).await;
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn docs(pool: &Pool, titles: &[&str]) -> Vec<i64> {
    let mut ids = Vec::new();
    for t in titles {
        let mut d = Doc {
            id: Auto::default(),
            title: (*t).to_owned(),
        };
        d.insert_pool(pool).await.expect("seed doc");
        ids.push(*d.id.get().expect("pk"));
    }
    ids
}

fn titles_of(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| r["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn doc_app(pool: &Pool, vs: impl FnOnce(ViewSet) -> ViewSet) -> axum::Router {
    vs(ViewSet::for_model(Doc::SCHEMA, &["id", "title"]).allow_anonymous())
        .router_pool("/docs", pool.clone())
}

/// Hidden reads as absent on every item route; a visible row a hook refuses is a 403.
async fn item_routes_hide_with_404_and_refuse_with_403(pool: &Pool) {
    let ids = docs(pool, &["hidden-a", "locked-b", "keep-c", "open-d"]).await;
    let app = doc_app(pool, |v| v);
    let (hidden, locked, keep, open) = (ids[0], ids[1], ids[2], ids[3]);
    let patch = Some(json!({ "title": "x" }));
    assert_eq!(
        send(&app, "GET", &format!("/docs/{hidden}"), None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "PATCH", &format!("/docs/{hidden}"), patch.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "DELETE", &format!("/docs/{hidden}"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "PUT", &format!("/docs/{locked}"), patch.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, "PATCH", &format!("/docs/{locked}"), patch.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, "DELETE", &format!("/docs/{keep}"), None).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = send(&app, "GET", &format!("/docs/{open}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/docs/{keep}"),
            Some(json!({ "title": "keep-2" }))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "DELETE", &format!("/docs/{open}"), None).await.0,
        StatusCode::NO_CONTENT
    );
    let left: Vec<String> = Doc::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .map(|d| d.title)
        .collect();
    assert!(
        left.contains(&"locked-b".to_owned()) && left.contains(&"keep-2".to_owned()),
        "{left:?}"
    );
    // An update that moves a row out of view answers 204, not the row.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/docs/{keep}"),
        Some(json!({ "title": "hidden-now" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// The soft-delete path runs the delete hook too.
async fn a_soft_delete_runs_the_delete_hook(pool: &Pool) {
    let mut row = Soft {
        id: Auto::default(),
        title: "keep-soft".into(),
        deleted_at: None,
    };
    row.insert_pool(pool).await.expect("seed");
    let id = *row.id.get().expect("pk");
    let app = ViewSet::for_model(Soft::SCHEMA, &["id", "title"])
        .allow_anonymous()
        .router_pool("/soft", pool.clone());
    assert_eq!(
        send(&app, "DELETE", &format!("/soft/{id}"), None).await.0,
        StatusCode::FORBIDDEN
    );
    let stored = Soft::objects().fetch(pool).await.expect("fetch");
    assert!(
        stored[0].deleted_at.is_none(),
        "a refused delete stamped the row"
    );
}

/// A page leaves hidden rows out of its rows and its count.
async fn a_page_counts_only_visible_rows(pool: &Pool) {
    docs(pool, &["a-1", "hidden-2", "a-3", "hidden-4", "a-5"]).await;
    let app = doc_app(pool, |v| v.page_size(2).ordering(&[("id", false)]));
    let (status, body) = send(&app, "GET", "/docs", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], 3, "{body}");
    assert_eq!(body["last_page"], 2, "{body}");
    assert_eq!(titles_of(&body), ["a-1", "a-3"]);
    let (_, body) = send(&app, "GET", "/docs?page=2", None).await;
    assert_eq!(titles_of(&body), ["a-5"]);
    let lo = doc_app(pool, |v| {
        v.limit_offset_pagination().ordering(&[("id", false)])
    });
    let (_, body) = send(&lo, "GET", "/docs?limit=2&offset=1", None).await;
    assert_eq!(body["count"], 3, "{body}");
    assert_eq!(titles_of(&body), ["a-3", "a-5"]);
}

/// Past the scan cap a page or offset list is refused, never a window
/// counted over hidden rows; a filter that fits the cap still works.
async fn past_the_scan_cap_offset_lists_are_refused(pool: &Pool) {
    docs(pool, &["a-1", "hidden-2", "a-3", "hidden-4", "a-5"]).await;
    let app = doc_app(pool, |v| {
        v.page_size(2)
            .view_hook_scan_cap(2)
            .filter_fields(&["title"])
            .ordering(&[("id", false)])
    });
    let (status, body) = send(&app, "GET", "/docs", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cursor pagination"),
        "{body}"
    );
    let (status, body) = send(&app, "GET", "/docs?title=a-3", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(titles_of(&body), ["a-3"]);
    let lo = doc_app(pool, |v| {
        v.limit_offset_pagination()
            .view_hook_scan_cap(2)
            .ordering(&[("id", false)])
    });
    let (status, body) = send(&lo, "GET", "/docs?limit=3&offset=2", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// A cursor page fills with visible rows; past the cap it still moves on.
async fn a_cursor_page_skips_hidden_rows(pool: &Pool) {
    docs(
        pool,
        &["a-1", "hidden-2", "hidden-3", "a-4", "a-5", "hidden-6"],
    )
    .await;
    for cap in [10_000, 2] {
        let app = doc_app(pool, |v| {
            v.page_size(2)
                .cursor_pagination("id")
                .view_hook_scan_cap(cap)
        });
        let mut seen = Vec::new();
        let mut uri = "/docs".to_owned();
        for _ in 0..10 {
            let (status, body) = send(&app, "GET", &uri, None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            seen.extend(titles_of(&body));
            match body["next"].as_str() {
                Some(next) => {
                    // The token names the last row shown, never a hidden one.
                    use base64::Engine as _;
                    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(next)
                        .expect("token");
                    let id: i64 = String::from_utf8(raw).unwrap().parse().expect("id");
                    let shown = Doc::objects().filter("id", id).fetch(pool).await.unwrap();
                    assert!(!shown[0].title.starts_with("hidden"), "cap {cap}: {id}");
                    uri = format!("/docs?cursor={next}");
                }
                None => break,
            }
        }
        assert_eq!(seen, ["a-1", "a-4", "a-5"], "cap {cap}");
    }
}

/// The opt-out skips the app's hooks and scopes.
async fn skip_object_permissions_skips_app_hooks(pool: &Pool) {
    let ids = docs(pool, &["hidden-a"]).await;
    let app = doc_app(pool, ViewSet::skip_object_permissions);
    assert_eq!(
        send(&app, "GET", &format!("/docs/{}", ids[0]), None)
            .await
            .0,
        StatusCode::OK
    );
    let (_, body) = send(&app, "GET", "/docs", None).await;
    assert_eq!(body["count"], 1, "{body}");
}

/// A queryset scope narrows every read and write.
async fn a_queryset_scope_narrows_every_action(pool: &Pool) {
    let mut ids = Vec::new();
    for owner in [1, 2] {
        let mut s = Scoped {
            id: Auto::default(),
            owner_id: owner,
            label_id: None,
        };
        s.insert_pool(pool).await.expect("seed");
        ids.push(*s.id.get().expect("pk"));
    }
    let app = ViewSet::for_model(Scoped::SCHEMA, &["id", "owner_id"])
        .allow_anonymous()
        .router_pool("/scoped", pool.clone());
    let (_, body) = send(&app, "GET", "/scoped", None).await;
    assert_eq!(body["count"], 1, "{body}");
    let other = ids[1];
    assert_eq!(
        send(&app, "GET", &format!("/scoped/{other}"), None).await.0,
        StatusCode::NOT_FOUND
    );
    let patch = Some(json!({ "owner_id": 1 }));
    assert_eq!(
        send(&app, "PATCH", &format!("/scoped/{other}"), patch)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "DELETE", &format!("/scoped/{other}"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(Scoped::objects().fetch(pool).await.expect("fetch").len(), 2);
}

/// An FK may not point at a row the target's "view" hook hides.
async fn an_fk_to_a_hidden_target_is_refused(pool: &Pool) {
    let mut label_ids = Vec::new();
    for name in ["secret", "public"] {
        let mut l = Label {
            id: Auto::default(),
            name: name.into(),
        };
        l.insert_pool(pool).await.expect("seed");
        label_ids.push(*l.id.get().expect("pk"));
    }
    let app = ViewSet::for_model(Card::SCHEMA, &["id", "label_id"])
        .allow_anonymous()
        .router_pool("/cards", pool.clone());
    let (status, body) = send(
        &app,
        "POST",
        "/cards",
        Some(json!({ "label_id": label_ids[0] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = send(
        &app,
        "POST",
        "/cards",
        Some(json!({ "label_id": label_ids[1] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// A superuser's row needs a superuser through the API too: an agent never is one,
/// and the opt-out does not skip this framework rule.
async fn a_superuser_row_needs_an_api_superuser(pool: &Pool) {
    let mut u = User {
        username: format!("vop{}", std::process::id()),
        is_superuser: true,
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed");
    let id = *u.id.get().expect("pk");
    let vs = |skip: bool| {
        let vs = ViewSet::for_model(User::SCHEMA, &["id", "username", "active"]).allow_anonymous();
        if skip {
            vs.skip_object_permissions()
        } else {
            vs
        }
    };
    let member = AuthenticatedUser {
        id: 999,
        username: "m".into(),
        is_superuser: false,
    };
    let superuser = AuthenticatedUser {
        is_superuser: true,
        ..member.clone()
    };
    let agent = Principal {
        user_id: 999,
        is_superuser: true,
        tenant: None,
        kind: PrincipalKind::Agent,
        agent_id: Some(5),
    };
    let patch = || Some(json!({ "active": false }));
    let uri = format!("/users/{id}");
    for skip in [false, true] {
        let app = vs(skip)
            .router_pool("/users", pool.clone())
            .layer(axum::Extension(member.clone()));
        assert_eq!(
            send(&app, "PATCH", &uri, patch()).await.0,
            StatusCode::FORBIDDEN,
            "skip {skip}"
        );
        assert_eq!(
            send(&app, "DELETE", &uri, None).await.0,
            StatusCode::FORBIDDEN,
            "skip {skip}"
        );
    }
    let app = vs(false)
        .router_pool("/users", pool.clone())
        .layer(axum::Extension(agent));
    assert_eq!(
        send(&app, "PATCH", &uri, patch()).await.0,
        StatusCode::FORBIDDEN
    );
    let app = vs(false)
        .router_pool("/users", pool.clone())
        .layer(axum::Extension(superuser));
    let (status, body) = send(&app, "PATCH", &uri, patch()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A page with more hidden than visible rows (`IN`), or only hidden rows.
async fn a_page_filters_by_admitted_or_by_nothing(pool: &Pool) {
    docs(pool, &["a-1", "hidden-2", "hidden-3", "hidden-4"]).await;
    let app = doc_app(pool, |v| v.ordering(&[("id", false)]));
    let (_, body) = send(&app, "GET", "/docs", None).await;
    assert_eq!(body["count"], 1, "{body}");
    assert_eq!(titles_of(&body), ["a-1"]);
    for mut d in Doc::objects().fetch(pool).await.expect("fetch") {
        d.title = format!("hidden-{}", d.id.get().unwrap());
        d.save_pool(pool).await.expect("hide");
    }
    let (_, body) = send(&app, "GET", "/docs", None).await;
    assert_eq!(body["count"], 0, "{body}");
    assert!(titles_of(&body).is_empty(), "{body}");
}

/// A row the hook hides after the list's scan is still left out of the page.
async fn a_row_hidden_after_the_scan_is_not_rendered(pool: &Pool) {
    FLIPPED.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut f = Flip {
        id: Auto::default(),
        title: "flip".into(),
    };
    f.insert_pool(pool).await.expect("seed");
    let app = ViewSet::for_model(Flip::SCHEMA, &["id", "title"])
        .allow_anonymous()
        .router_pool("/flip", pool.clone());
    let (status, body) = send(&app, "GET", "/flip", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(titles_of(&body).is_empty(), "{body}");
}

/// The deprecated macro names still register their hook and scope.
async fn the_old_macro_names_still_apply(pool: &Pool) {
    for t in ["a", "hidden-b", "out"] {
        let mut l = Legacy {
            id: Auto::default(),
            title: t.into(),
        };
        l.insert_pool(pool).await.expect("seed");
    }
    let app = ViewSet::for_model(Legacy::SCHEMA, &["id", "title"])
        .allow_anonymous()
        .router_pool("/legacy", pool.clone());
    let (_, body) = send(&app, "GET", "/legacy", None).await;
    assert_eq!(titles_of(&body), ["a"], "{body}");
}

/// A write may not move a row out of, or create one outside, the queryset scopes.
async fn a_write_cannot_leave_the_queryset_scope(pool: &Pool) {
    let mut own = Scoped {
        id: Auto::default(),
        owner_id: 1,
        label_id: None,
    };
    own.insert_pool(pool).await.expect("seed");
    let id = *own.id.get().expect("pk");
    let app = ViewSet::for_model(Scoped::SCHEMA, &["id", "owner_id"])
        .allow_anonymous()
        .router_pool("/scoped", pool.clone());
    let moved = Some(json!({ "owner_id": 2 }));
    assert_eq!(
        send(&app, "PATCH", &format!("/scoped/{id}"), moved.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, "POST", "/scoped", moved).await.0,
        StatusCode::FORBIDDEN
    );
    let rows = Scoped::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner_id, 1, "a refused update wrote");
}

/// A hidden row is a 404 before its body's FK is checked: a 422 would confirm it.
async fn a_hidden_row_is_404_before_its_fk_check(pool: &Pool) {
    let mut secret = Label {
        id: Auto::default(),
        name: "secret".into(),
    };
    secret.insert_pool(pool).await.expect("seed");
    let mut other = Scoped {
        id: Auto::default(),
        owner_id: 2,
        label_id: None,
    };
    other.insert_pool(pool).await.expect("seed");
    let app = ViewSet::for_model(Scoped::SCHEMA, &["id", "owner_id", "label_id"])
        .allow_anonymous()
        .router_pool("/scoped", pool.clone());
    let uri = format!("/scoped/{}", other.id.get().unwrap());
    let body = Some(json!({ "label_id": secret.id.get().unwrap() }));
    assert_eq!(
        send(&app, "PATCH", &uri, body).await.0,
        StatusCode::NOT_FOUND
    );
}

/// A concurrent edit is judged after it lands: the hooks read the row
/// locked in the write's transaction. SQLite already serializes writers.
async fn a_concurrent_edit_is_judged_after_it_lands(pool: &Pool) {
    use rustango::core::{Assignment, UpdateQuery, WhereExpr};
    if pool.dialect().name() == "sqlite" {
        return;
    }
    let ids = docs(pool, &["open-1", "open-2"]).await;
    let app = doc_app(pool, |v| v);
    for (id, title, method) in [(ids[0], "locked-1", "PATCH"), (ids[1], "keep-2", "DELETE")] {
        let mut tx = rustango::sql::write_transaction(pool).await.expect("tx");
        let edit = UpdateQuery::new(
            Doc::SCHEMA,
            vec![Assignment::new("title", SqlValue::from(title))],
            WhereExpr::Predicate(Filter::new("id", Op::Eq, SqlValue::I64(id))),
        );
        rustango::sql::update_tx(&mut tx, &edit)
            .await
            .expect("edit");
        let app = app.clone();
        let uri = format!("/docs/{id}");
        let call = tokio::spawn(async move {
            let body = (method == "PATCH").then(|| json!({ "title": "x" }));
            send(&app, method, &uri, body).await.0
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        tx.commit().await.expect("commit");
        assert_eq!(call.await.unwrap(), StatusCode::FORBIDDEN, "{method}");
    }
}

/// A PATCH whose body is still uploading holds no lock: another writer
/// changes the same row meanwhile.
async fn a_pending_body_holds_no_lock(pool: &Pool) {
    let id = docs(pool, &["open-slow"]).await[0];
    let (send_body, body_ready) = tokio::sync::oneshot::channel::<()>();
    let body = Body::from_stream(futures_util::stream::once(async move {
        let _ = body_ready.await;
        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(br#"{"title":"open-late"}"#))
    }));
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/docs/{id}"))
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let call = tokio::spawn(doc_app(pool, |v| v).oneshot(req));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut doc = Doc::objects()
        .filter("id", id)
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    doc.title = "open-other".into();
    let saved = tokio::time::timeout(std::time::Duration::from_secs(3), doc.save_pool(pool)).await;
    assert!(
        matches!(saved, Ok(Ok(_))),
        "a pending body held the write lock: {saved:?}"
    );
    send_body.send(()).unwrap();
    assert_eq!(call.await.unwrap().unwrap().status(), StatusCode::OK);
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        a_pending_body_holds_no_lock,
        a_page_filters_by_admitted_or_by_nothing,
        a_row_hidden_after_the_scan_is_not_rendered,
        the_old_macro_names_still_apply,
        a_write_cannot_leave_the_queryset_scope,
        a_hidden_row_is_404_before_its_fk_check,
        a_concurrent_edit_is_judged_after_it_lands,
        item_routes_hide_with_404_and_refuse_with_403,
        a_soft_delete_runs_the_delete_hook,
        a_page_counts_only_visible_rows,
        past_the_scan_cap_offset_lists_are_refused,
        a_cursor_page_skips_hidden_rows,
        skip_object_permissions_skips_app_hooks,
        a_queryset_scope_narrows_every_action,
        an_fk_to_a_hidden_target_is_refused,
        a_superuser_row_needs_an_api_superuser,
    ],
);
