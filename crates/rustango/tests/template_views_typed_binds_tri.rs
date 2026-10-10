//! Template views bind form, filter and FK-display values by field type, not as text (#1915).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "template_views"
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::template_views::{CreateView, DeleteView, DetailView, ListView, UpdateView};
use rustango::{tri_dialect_test, Model};
use tera::Tera;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "tv1915_event", app = "tv1915")]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub day: chrono::NaiveDate,
    pub at: chrono::DateTime<chrono::Utc>,
    pub token: uuid::Uuid,
    pub meta: serde_json::Value,
    pub done: bool,
    pub author_id: i64,
    #[rustango(max_length = 32)]
    pub note: Option<String>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tv1915_tag", app = "tv1915", display = "name")]
pub struct Tag {
    #[rustango(primary_key, max_length = 64)]
    pub code: String,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tv1915_pin", app = "tv1915")]
pub struct Pin {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, on = "code")]
    pub tag: ForeignKey<Tag, String>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tv1915_label", app = "tv1915")]
pub struct Label {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(unique, max_length = 32)]
    pub code: String,
}

/// A composite UNIQUE: no single field to blame for a clash.
#[derive(Model, Debug, Clone)]
#[rustango(table = "tv2073_slot", app = "tv1915", unique_together = "day, hour")]
pub struct Slot {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub day: String,
    pub hour: i64,
}

/// `fresh_table` skips composite-unique indexes, so render Slot's here.
async fn slot_unique_index(pool: &Pool) {
    use rustango::migrate::{render_changes_split_with_dialect, SchemaChange, SchemaSnapshot};
    let change = SchemaChange::CreateIndex {
        name: "uq_tv2073_slot_day_hour".into(),
        table: Slot::SCHEMA.table.into(),
        columns: vec!["day".into(), "hour".into()],
        unique: true,
        method: "btree".into(),
        where_clause: None,
        include: Vec::new(),
    };
    let snap = SchemaSnapshot::from_models(&[]);
    let sql = render_changes_split_with_dialect(&[change], &snap, pool.dialect())
        .expect("render the slot index");
    for stmt in sql.immediate {
        rustango::sql::raw_execute_pool(pool, &stmt, Vec::new())
            .await
            .expect("create the slot index");
    }
}

/// A Rust-filled v7 PK (#1725).
#[derive(Model, Debug, Clone)]
#[rustango(table = "tv1725_doc", app = "tv1915")]
pub struct Doc {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<uuid::Uuid>,
    #[rustango(max_length = 32)]
    pub name: String,
}

/// `auto_now` beside `auto_now_add` (#2527).
#[derive(Model, Debug, Clone)]
#[rustango(table = "tv2527_memo", app = "tv1915")]
pub struct Memo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub title: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

const CSRF: &str = "tv1915-csrf-token-tv1915-csrf-token-tv1915x";
const UUID_A: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const FORM: &str = "application/x-www-form-urlencoded";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Pin::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Event>(pool).await;
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Pin>(pool).await;
    rustango::testkit::matrix::fresh_table::<Label>(pool).await;
    rustango::testkit::matrix::fresh_table::<Slot>(pool).await;
    slot_unique_index(pool).await;
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
    rustango::testkit::matrix::fresh_table::<Memo>(pool).await;
}

fn app(pool: &Pool) -> axum::Router {
    let mut t = Tera::default();
    t.add_raw_templates(vec![
        ("form.html", "form {{ form.errors | json_encode() | safe }}"),
        ("list.html", "rows={{ object_list | length }}"),
        ("detail.html", "event"),
        (
            "pins.html",
            r#"{% for r in object_list %}{{ r.tag_display | default(value="-") }};{% endfor %}"#,
        ),
    ])
    .unwrap();
    let t = Arc::new(t);
    CreateView::for_model(Event::SCHEMA)
        .template("form.html")
        .success_url("/events")
        .router("/events", t.clone(), pool.clone())
        .merge(
            UpdateView::for_model(Event::SCHEMA)
                .template("form.html")
                .success_url("/events")
                .router("/events", t.clone(), pool.clone()),
        )
        .merge(
            ListView::for_model(Event::SCHEMA)
                .template("list.html")
                .filter_fields(&["author_id", "done", "token", "day", "note", "at"])
                .router("/events", t.clone(), pool.clone()),
        )
        .merge(
            DeleteView::for_model(Event::SCHEMA)
                .template("detail.html")
                .success_url("/events")
                .router("/events", t.clone(), pool.clone()),
        )
        .merge(
            DetailView::for_model(Event::SCHEMA)
                .template("detail.html")
                .router("/ev", t.clone(), pool.clone()),
        )
        .merge(
            CreateView::for_model(Label::SCHEMA)
                .template("form.html")
                .success_url("/labels")
                .router("/labels", t.clone(), pool.clone()),
        )
        .merge(
            CreateView::for_model(Slot::SCHEMA)
                .template("form.html")
                .success_url("/slots")
                .router("/slots", t.clone(), pool.clone()),
        )
        .merge(
            UpdateView::for_model(Slot::SCHEMA)
                .template("form.html")
                .success_url("/slots")
                .router("/slots", t.clone(), pool.clone()),
        )
        .merge(
            UpdateView::for_model(Label::SCHEMA)
                .template("form.html")
                .success_url("/labels")
                .router("/labels", t.clone(), pool.clone()),
        )
        .merge(
            CreateView::for_model(Label::SCHEMA)
                .template("form.html")
                .success_url("/labels/{id}")
                .router("/rlabels", t.clone(), pool.clone()),
        )
        .merge(
            CreateView::for_model(Tag::SCHEMA)
                .template("form.html")
                .success_url("/tags")
                .router("/tags", t.clone(), pool.clone()),
        )
        .merge(
            CreateView::for_model(Doc::SCHEMA)
                .template("form.html")
                .success_url("/docs")
                .router("/docs", t.clone(), pool.clone()),
        )
        .merge(
            UpdateView::for_model(Memo::SCHEMA)
                .template("form.html")
                .success_url("/memos")
                .router("/memos", t.clone(), pool.clone()),
        )
        .merge(
            ListView::for_model(Pin::SCHEMA)
                .template("pins.html")
                .with_fk_display(true)
                .router("/pins", t, pool.clone()),
        )
}

async fn send(pool: &Pool, method: Method, uri: &str, body: &str) -> (StatusCode, String) {
    let resp = app(pool)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, FORM)
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
                .body(Body::from(format!("_csrf={CSRF}&{body}")))
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

fn form(day: &str, author: i64, done: bool) -> String {
    format!(
        "day={day}&at=2026-09-29T10%3A30%3A00Z&token={UUID_A}\
         &meta=%7B%22k%22%3A1%7D&done={done}&author_id={author}"
    )
}

async fn events(pool: &Pool) -> Vec<Event> {
    Event::objects().fetch(pool).await.expect("fetch")
}

async fn create_and_update_bind_typed_values(pool: &Pool) {
    let (status, body) = send(
        pool,
        Method::POST,
        "/events/new",
        &form("2026-09-29", 7, true),
    )
    .await;
    assert!(status.is_redirection(), "create: {status} {body}");
    let rows = events(pool).await;
    assert_eq!(rows.len(), 1);
    let e = &rows[0];
    assert_eq!(e.day.to_string(), "2026-09-29");
    assert_eq!(e.at.to_rfc3339(), "2026-09-29T10:30:00+00:00");
    assert_eq!(e.token.to_string(), UUID_A);
    assert_eq!(e.meta, serde_json::json!({"k": 1}));
    assert!(e.done);

    let pk = *e.id.get().expect("pk");
    let uri = format!("/events/{pk}/edit");
    let (status, body) = send(pool, Method::POST, &uri, &form("2026-10-01", 8, false)).await;
    assert!(status.is_redirection(), "update: {status} {body}");
    let e = &events(pool).await[0];
    assert_eq!(e.day.to_string(), "2026-10-01");
    assert_eq!(e.author_id, 8);
    assert!(!e.done);
}

async fn list_filters_bind_typed_values(pool: &Pool) {
    for (day, author, done) in [("2026-09-29", 7, true), ("2026-09-30", 9, false)] {
        let (status, body) =
            send(pool, Method::POST, "/events/new", &form(day, author, done)).await;
        assert!(status.is_redirection(), "seed: {status} {body}");
    }
    for (uri, want) in [
        ("/events?author_id=7", "rows=1"),
        ("/events?done=true", "rows=1"),
        ("/events?done=false", "rows=1"),
        ("/events?day=2026-09-30", "rows=1"),
        (&*format!("/events?token={UUID_A}"), "rows=2"),
        // Empty and unparsable values are ignored, not `= NULL` / `true`.
        ("/events?note=", "rows=2"),
        ("/events?done=banana", "rows=2"),
        ("/events?done=on", "rows=1"),
        ("/events?at=2020-01-01%2000%3A00%3A00", "rows=0"),
        ("/events?at=2026-09-29%2010%3A30%3A00", "rows=2"),
    ] {
        let (status, body) = send(pool, Method::GET, uri, "").await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body, want, "{uri}");
    }
}

/// A string PK that looks like a UUID must bind as text, not uuid.
async fn fk_display_binds_the_target_pk_type(pool: &Pool) {
    let tag = Tag {
        code: UUID_A.into(),
        name: "Rust".into(),
    };
    tag.insert_pool(pool).await.expect("tag");
    let mut pin = Pin {
        id: Auto::Unset,
        tag: ForeignKey::unloaded(UUID_A.to_owned()),
    };
    pin.insert_pool(pool).await.expect("pin");
    let (status, body) = send(pool, Method::GET, "/pins", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "Rust;");
}

/// A URL PK that does not parse as the PK type is a 404, not a PG cast 500 (#1950).
async fn garbage_url_pk_is_a_404(pool: &Pool) {
    let (status, body) = send(pool, Method::GET, "/ev/not-a-number", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send(
        pool,
        Method::POST,
        "/events/abc/edit",
        &form("2026-10-01", 1, true),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    for (method, uri) in [
        (Method::GET, "/events/abc/edit"),
        (Method::GET, "/events/abc/delete"),
        (Method::POST, "/events/abc/delete"),
    ] {
        let (status, body) = send(pool, method.clone(), uri, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
    }
}

/// `?page=i64::MAX` is an empty page on every paging path, not an overflow (#1865).
async fn huge_page_is_an_empty_page(pool: &Pool) {
    let (status, body) = send(
        pool,
        Method::POST,
        "/events/new",
        &form("2026-09-29", 7, true),
    )
    .await;
    assert!(status.is_redirection(), "seed: {status} {body}");
    let (status, body) = send(pool, Method::GET, &format!("/events?page={}", i64::MAX), "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "rows=0"));
    let (rows, total) = Event::objects()
        .paginate(i64::MAX, 20, pool)
        .await
        .expect("paginate");
    assert_eq!((rows.len(), total), (0, 1));
    assert!(Event::for_page(i64::MAX, 20, pool)
        .await
        .expect("for_page")
        .is_empty());
}

/// A duplicate unique value re-renders the form with a field error (#2033),
/// without the driver text (#1955).
async fn a_duplicate_create_is_a_form_error(pool: &Pool) {
    let (status, body) = send(pool, Method::POST, "/labels/new", "code=rs").await;
    assert!(status.is_redirection(), "first create: {status} {body}");
    let (status, body) = send(pool, Method::POST, "/labels/new", "code=rs").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body, r#"form {"code":"a row with this value already exists"}"#,
        "{body}"
    );
    let (status, body) = send(pool, Method::POST, "/tags/new", "code=go&name=Go").await;
    assert!(status.is_redirection(), "first tag: {status} {body}");
    let (status, body) = send(pool, Method::POST, "/tags/new", "code=go&name=G2").await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "natural pk: {body}"
    );
    assert!(body.contains(r#""code":"a row with this value"#), "{body}");
    // The RETURNING path (`{id}` in `success_url`) maps it the same way.
    let (status, body) = send(pool, Method::POST, "/rlabels/new", "code=rs").await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "returning: {body}"
    );
    assert!(body.contains(r#""code":"a row with this value"#), "{body}");
}

/// UpdateView answers a taken unique value with a form error, not a 500 (#2073).
async fn a_duplicate_update_is_a_form_error(pool: &Pool) {
    for code in ["rs", "go"] {
        let (status, body) = send(pool, Method::POST, "/labels/new", &format!("code={code}")).await;
        assert!(status.is_redirection(), "seed {code}: {status} {body}");
    }
    let go = Label::objects()
        .filter("code", "go")
        .fetch(pool)
        .await
        .expect("labels")[0]
        .id
        .get()
        .copied()
        .expect("pk");
    let edit = format!("/labels/{go}/edit");
    let (status, body) = send(pool, Method::POST, &edit, "code=rs").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body, r#"form {"code":"a row with this value already exists"}"#,
        "{body}"
    );
    // Keeping its own value is not a duplicate.
    let (status, body) = send(pool, Method::POST, &edit, "code=go").await;
    assert!(status.is_redirection(), "own value: {status} {body}");
}

/// A composite-UNIQUE clash is a form-wide error on create and update (#2033, #2073).
async fn a_composite_unique_clash_is_a_form_wide_error(pool: &Pool) {
    let all = r#"form {"__all__":"a row with this value already exists"}"#;
    for form in ["day=mon&hour=9", "day=mon&hour=10"] {
        let (status, body) = send(pool, Method::POST, "/slots/new", form).await;
        assert!(status.is_redirection(), "seed {form}: {status} {body}");
    }
    let (status, body) = send(pool, Method::POST, "/slots/new", "day=mon&hour=9").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "create: {body}");
    assert_eq!(body, all, "create");
    let ten = Slot::objects()
        .filter("hour", 10_i64)
        .fetch(pool)
        .await
        .expect("slots")[0]
        .id
        .get()
        .copied()
        .expect("pk");
    let edit = format!("/slots/{ten}/edit");
    let (status, body) = send(pool, Method::POST, &edit, "day=mon&hour=9").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "update: {body}");
    assert_eq!(body, all, "update");
}

/// CreateView inserts a natural PK from the form and fills a v7 one (#1725).
async fn create_view_writes_natural_and_v7_pks(pool: &Pool) {
    let (status, body) = send(pool, Method::POST, "/tags/new", "code=go&name=Go").await;
    assert!(status.is_redirection(), "natural pk: {status} {body}");
    let tags: Vec<Tag> = Tag::objects().fetch(pool).await.expect("tags");
    assert_eq!(tags.len(), 1);
    assert_eq!((tags[0].code.as_str(), tags[0].name.as_str()), ("go", "Go"));

    let (status, body) = send(pool, Method::POST, "/docs/new", "name=D").await;
    assert!(status.is_redirection(), "v7 pk: {status} {body}");
    let docs: Vec<Doc> = Doc::objects().fetch(pool).await.expect("docs");
    assert_eq!(docs.len(), 1);
    let id = *docs[0].id.get().expect("pk");
    assert_eq!(id.get_version_num(), 7, "{id}");
}

/// UpdateView restamps `auto_now` and leaves `auto_now_add` (#2527).
async fn update_view_restamps_auto_now(pool: &Pool) {
    use rustango::core::{Assignment, Filter, Op, SqlValue, UpdateQuery, WhereExpr};
    let mut memo = Memo {
        id: Auto::Unset,
        title: "a".into(),
        created_at: Auto::Unset,
        updated_at: Auto::Unset,
    };
    memo.insert_pool(pool).await.expect("seed");
    let id = *memo.id.get().expect("pk");
    let old = chrono::DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let age = UpdateQuery::new(
        Memo::SCHEMA,
        vec![
            Assignment::new("created_at", SqlValue::DateTime(old)),
            Assignment::new("updated_at", SqlValue::DateTime(old)),
        ],
        WhereExpr::Predicate(Filter::new("id", Op::Eq, SqlValue::I64(id))),
    );
    rustango::sql::update_pool(pool, &age)
        .await
        .expect("age the row");

    let (status, body) = send(pool, Method::POST, &format!("/memos/{id}/edit"), "title=b").await;
    assert!(status.is_redirection(), "{status} {body}");
    let memos: Vec<Memo> = Memo::objects().fetch(pool).await.expect("memos");
    assert_eq!(memos[0].title, "b");
    assert_eq!(memos[0].created_at.get(), Some(&old), "auto_now_add moved");
    assert!(memos[0].updated_at.get() > Some(&old), "auto_now stale");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        update_view_restamps_auto_now,
        a_duplicate_create_is_a_form_error,
        a_duplicate_update_is_a_form_error,
        a_composite_unique_clash_is_a_form_wide_error,
        create_and_update_bind_typed_values,
        list_filters_bind_typed_values,
        fk_display_binds_the_target_pk_type,
        garbage_url_pk_is_a_404,
        huge_page_is_an_empty_page,
        create_view_writes_natural_and_v7_pks,
    ],
}
