//! Admin writes on every backend: an FK may only name a target row the
//! user can see (create, edit, inlines); autocomplete reads renamed
//! columns; audit files a row under its key's canonical text; a blocked
//! delete answers a browser with a page.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

/// The queryset hook shows owner 1's live projects only.
#[derive(Model, Debug, Clone)]
#[rustango(table = "wt_project", display = "name")]
#[allow(dead_code)]
pub struct Project {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub owner_id: i64,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn owner_one(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("owner_id", Op::Eq, SqlValue::I64(1))]
}
rustango::register_admin_queryset!("wt_project", owner_one);

/// The "view" hook hides `secret*` labels.
#[derive(Model, Debug, Clone)]
#[rustango(table = "wt_label", display = "name")]
#[allow(dead_code)]
pub struct Label {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

fn not_secret(_: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    row.and_then(|r| r["name"].as_str())
        .is_none_or(|n| !n.starts_with("secret"))
}
rustango::register_admin_object_permission!("wt_label", "view", not_secret);

#[derive(Model, Debug, Clone)]
#[rustango(table = "wt_sprint", display = "name")]
#[allow(dead_code)]
pub struct Sprint {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "wt_task")]
#[allow(dead_code)]
pub struct Task {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "wt_sprint", on = "id")]
    pub sprint_id: i64,
    #[rustango(fk = "wt_project", on = "id")]
    pub project_id: i64,
    #[rustango(fk = "wt_label", on = "id")]
    pub label_id: Option<i64>,
    #[rustango(max_length = 32)]
    pub title: String,
}

rustango::register_admin_inline!(
    parent = "wt_sprint",
    child = "wt_task",
    fk = "sprint_id",
    fields = &["project_id", "title"],
    extra = 1,
);

/// Renamed PK and display columns.
#[derive(Model, Debug, Clone)]
#[rustango(table = "wt_tag", display = "name")]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key, column = "tag_pk")]
    pub id: Auto<i64>,
    #[rustango(max_length = 32, column = "tag_label")]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "wt_note")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub title: String,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "wt_task").await;
    fresh_table::<Project>(pool).await;
    fresh_table::<Label>(pool).await;
    fresh_table::<Sprint>(pool).await;
    fresh_table::<Task>(pool).await;
    fresh_table::<Tag>(pool).await;
    fresh_table::<Note>(pool).await;
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
    rustango::audit::AuditLog::delete_where("entity_table", "wt_note", pool)
        .await
        .expect("clear audit rows");
}

struct Reply {
    status: StatusCode,
    content_type: String,
    body: String,
}

async fn send(pool: &Pool, req: axum::http::request::Builder, body: String) -> Reply {
    send_as(rustango::admin::Builder::new(pool.clone()), req, body).await
}

async fn send_as(
    admin: rustango::admin::Builder,
    req: axum::http::request::Builder,
    body: String,
) -> Reply {
    let app = admin.admin_prefix("").build();
    let res = app
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn form_post(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
}

async fn post(pool: &Pool, uri: &str, form: &str) -> Reply {
    send(pool, form_post(uri), form.to_owned()).await
}

async fn project(pool: &Pool, name: &str, owner_id: i64, deleted: bool) -> i64 {
    let mut p = Project {
        id: Auto::default(),
        name: name.into(),
        owner_id,
        deleted_at: deleted.then(chrono::Utc::now),
    };
    p.insert_pool(pool).await.expect("insert project");
    *p.id.get().expect("pk")
}

async fn label(pool: &Pool, name: &str) -> i64 {
    let mut l = Label {
        id: Auto::default(),
        name: name.into(),
    };
    l.insert_pool(pool).await.expect("insert label");
    *l.id.get().expect("pk")
}

async fn sprint(pool: &Pool, name: &str) -> i64 {
    let mut s = Sprint {
        id: Auto::default(),
        name: name.into(),
    };
    s.insert_pool(pool).await.expect("insert sprint");
    *s.id.get().expect("pk")
}

async fn task(pool: &Pool, sprint_id: i64, project_id: i64, title: &str) -> i64 {
    let mut t = Task {
        id: Auto::default(),
        sprint_id,
        project_id,
        label_id: None,
        title: title.into(),
    };
    t.insert_pool(pool).await.expect("insert task");
    *t.id.get().expect("pk")
}

async fn tasks(pool: &Pool) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = Task::objects()
        .fetch(pool)
        .await
        .expect("fetch tasks")
        .into_iter()
        .map(|t| (t.title, t.project_id))
        .collect();
    v.sort();
    v
}

fn assert_refused(r: &Reply, what: &str) {
    assert_eq!(r.status, StatusCode::OK, "{what}: {}", r.body);
    assert!(r.body.contains("no such object"), "{what}: {}", r.body);
}

/// A create may not link a project outside the hook's scope, a
/// soft-deleted one, or a label the "view" hook hides.
async fn create_refuses_hidden_fk_targets(pool: &Pool) {
    let s = sprint(pool, "s").await;
    let mine = project(pool, "mine", 1, false).await;
    let theirs = project(pool, "theirs", 77, false).await;
    let gone = project(pool, "gone", 1, true).await;
    let open = label(pool, "open").await;
    let secret = label(pool, "secret").await;
    for (p, l, what) in [
        (theirs, open, "other owner"),
        (gone, open, "soft-deleted"),
        (mine, secret, "view hook"),
    ] {
        let form = format!("sprint_id={s}&project_id={p}&label_id={l}&title=t");
        assert_refused(&post(pool, "/wt_task", &form).await, what);
    }
    assert!(tasks(pool).await.is_empty(), "a hidden link was saved");
    // Control: visible targets save.
    let form = format!("sprint_id={s}&project_id={mine}&label_id={open}&title=ok");
    let r = post(pool, "/wt_task", &form).await;
    assert!(r.status.is_redirection(), "{}: {}", r.status, r.body);
    assert_eq!(tasks(pool).await, [("ok".to_owned(), mine)]);
}

/// An edit may keep a link it already holds, but not make a new hidden one.
async fn update_refuses_a_new_hidden_fk_target(pool: &Pool) {
    let s = sprint(pool, "s").await;
    let theirs = project(pool, "theirs", 77, false).await;
    let other = project(pool, "other", 77, false).await;
    let t = task(pool, s, theirs, "t").await;
    let kept = format!("sprint_id={s}&project_id={theirs}&title=kept");
    let r = post(pool, &format!("/wt_task/{t}"), &kept).await;
    assert!(r.status.is_redirection(), "{}: {}", r.status, r.body);
    let moved = format!("sprint_id={s}&project_id={other}&title=moved");
    assert_refused(&post(pool, &format!("/wt_task/{t}"), &moved).await, "edit");
    assert_eq!(tasks(pool).await, [("kept".to_owned(), theirs)]);
}

/// Inline rows obey the same rule, on insert and on edit.
async fn inline_refuses_hidden_fk_targets(pool: &Pool) {
    let s = sprint(pool, "s").await;
    let mine = project(pool, "mine", 1, false).await;
    let theirs = project(pool, "theirs", 77, false).await;
    let insert = format!(
        "name=s&wt_task-TOTAL_FORMS=1&wt_task-INITIAL_FORMS=0\
         &wt_task-0-project_id={theirs}&wt_task-0-title=new"
    );
    assert_refused(
        &post(pool, &format!("/wt_sprint/{s}"), &insert).await,
        "insert",
    );
    assert!(
        tasks(pool).await.is_empty(),
        "a hidden inline link was saved"
    );

    let t = task(pool, s, mine, "t").await;
    let edit = format!(
        "name=s&wt_task-TOTAL_FORMS=1&wt_task-INITIAL_FORMS=1\
         &wt_task-0-id={t}&wt_task-0-project_id={theirs}&wt_task-0-title=t"
    );
    assert_refused(&post(pool, &format!("/wt_sprint/{s}"), &edit).await, "edit");
    assert_eq!(tasks(pool).await, [("t".to_owned(), mine)]);
}

/// Inline FK checks read each target once, not twice per changed row.
async fn inline_fk_checks_read_each_target_once(pool: &Pool) {
    use rustango::test_assertions::QueryCounter;
    let s = sprint(pool, "s").await;
    let mine = project(pool, "mine", 1, false).await;
    let next = project(pool, "next", 1, false).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(task(pool, s, mine, &format!("t{i}")).await);
    }
    let form = |project: i64, title: &str| {
        let mut f = String::from("name=s&wt_task-TOTAL_FORMS=3&wt_task-INITIAL_FORMS=3");
        for (i, id) in ids.iter().enumerate() {
            f.push_str(&format!(
                "&wt_task-{i}-id={id}&wt_task-{i}-project_id={project}&wt_task-{i}-title={title}{i}"
            ));
        }
        f
    };
    let counted = |body: String| async move {
        QueryCounter::scope(async {
            let r = post(pool, &format!("/wt_sprint/{s}"), &body).await;
            assert!(r.status.is_redirection(), "{}: {}", r.status, r.body);
            QueryCounter::current()
        })
        .await
    };
    // Same writes either way; only the FK checks differ.
    let kept = counted(form(mine, "a")).await;
    let moved = counted(form(next, "b")).await;
    assert_eq!(moved - kept, 1, "kept {kept}, moved {moved}");
    assert_eq!(
        tasks(pool).await.iter().filter(|(_, p)| *p == next).count(),
        3
    );
    // The refusal still names the row that links a hidden target.
    let theirs = project(pool, "theirs", 77, false).await;
    let mixed = format!(
        "name=s&wt_task-TOTAL_FORMS=2&wt_task-INITIAL_FORMS=2\
         &wt_task-0-id={}&wt_task-0-project_id={mine}&wt_task-0-title=c\
         &wt_task-1-id={}&wt_task-1-project_id={theirs}&wt_task-1-title=c",
        ids[0], ids[1]
    );
    let r = post(pool, &format!("/wt_sprint/{s}"), &mixed).await;
    assert_refused(&r, "mixed");
    assert!(r.body.contains(&format!("row {}", ids[1])), "{}", r.body);
}

/// Autocomplete reads a renamed PK and display column.
async fn autocomplete_reads_renamed_columns(pool: &Pool) {
    let mut tag = Tag {
        id: Auto::default(),
        name: "alpha".into(),
    };
    tag.insert_pool(pool).await.expect("insert tag");
    let id = *tag.id.get().expect("pk");
    let r = send(
        pool,
        Request::builder().uri("/wt_tag/__autocomplete?q="),
        String::new(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert_eq!(
        v["results"],
        serde_json::json!([{ "id": id, "text": "alpha" }]),
        "{}",
        r.body
    );
}

async fn note_entries(pool: &Pool, pk: &str) -> Vec<String> {
    rustango::audit::fetch_for_entity_pool(pool, "wt_note", pk)
        .await
        .expect("audit entries")
        .into_iter()
        .map(|e| e.operation)
        .collect()
}

/// `/wt_note/007` and `/wt_note/7` are one row with one history.
async fn audit_files_the_canonical_key(pool: &Pool) {
    let mut n = Note {
        id: Auto::default(),
        title: "n".into(),
    };
    n.insert_pool(pool).await.expect("insert note");
    let id = *n.id.get().expect("pk");
    let r = post(pool, &format!("/wt_note/00{id}"), "title=zeroed").await;
    assert!(r.status.is_redirection(), "{}: {}", r.status, r.body);
    assert_eq!(note_entries(pool, &id.to_string()).await.len(), 1);

    // The detail page finds it whichever way the URL spells the key.
    let viewer = || {
        rustango::admin::Builder::new(pool.clone()).with_user_perms([
            rustango::audit::VIEW_CODENAME.to_owned(),
            "wt_note.view".to_owned(),
        ])
    };
    for uri in [format!("/wt_note/{id}"), format!("/wt_note/00{id}")] {
        let r = send_as(viewer(), Request::builder().uri(&uri), String::new()).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.body);
        assert!(r.body.contains("audit-op-update"), "{uri}: {}", r.body);
    }

    let r = post(pool, &format!("/wt_note/00{id}/delete"), "").await;
    assert!(r.status.is_redirection(), "{}: {}", r.status, r.body);
    assert_eq!(note_entries(pool, &id.to_string()).await.len(), 2);
}

/// A blocked delete is a page for a browser and JSON for others, both 409.
async fn blocked_delete_is_a_page_for_a_browser(pool: &Pool) {
    let s = sprint(pool, "s").await;
    let mine = project(pool, "mine", 1, false).await;
    task(pool, s, mine, "t").await;
    let uri = format!("/wt_sprint/{s}/delete");
    let browser = form_post(&uri).header(header::ACCEPT, "text/html,application/xhtml+xml");
    let r = send(pool, browser, String::new()).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert!(
        r.content_type.starts_with("text/html"),
        "{}",
        r.content_type
    );
    assert!(r.body.contains("still referenced"), "{}", r.body);
    assert!(r.body.contains("Cannot delete"), "{}", r.body);

    let api = form_post(&uri).header(header::ACCEPT, "application/json");
    let r = send(pool, api, String::new()).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert!(r.content_type.contains("json"), "{}", r.content_type);
    assert!(r.body.contains("referenced_by"), "{}", r.body);
    assert_eq!(Sprint::objects().fetch(pool).await.unwrap().len(), 1);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        create_refuses_hidden_fk_targets,
        update_refuses_a_new_hidden_fk_target,
        inline_refuses_hidden_fk_targets,
        inline_fk_checks_read_each_target_once,
        autocomplete_reads_renamed_columns,
        audit_files_the_canonical_key,
        blocked_delete_is_a_page_for_a_browser,
    ],
}
