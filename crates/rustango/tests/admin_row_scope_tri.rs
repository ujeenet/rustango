//! `register_queryset_scope!` scopes by-pk reads, facet counts and FK
//! facet labels and FK cells on every backend (#1859, #2029, #2080); inlines hide secrets and
//! rows the view hook refuses, cap rows and insert natural PKs (#1861, #1717).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
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
rustango::register_queryset_scope!("rowscope_item", owner_one);

fn not_locked_title(_: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    row.and_then(|r| r.get("title")) != Some(&serde_json::json!("locked"))
}
rustango::register_object_permission!("rowscope_item", "add", not_locked_title);
rustango::register_object_permission!("rowscope_item", "change", not_locked_title);

/// An FK facet onto `rowscope_item`, whose hook hides owner 77.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_note",
    admin(list_display = "body, item_id", list_filter = "item_id")
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
rustango::register_object_permission!("rowscope_child", "view", not_hidden);

fn not_out(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("label", Op::Ne, SqlValue::String("out".into()))]
}
rustango::register_queryset_scope!("rowscope_child", not_out);

fn not_noadd(_: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    row.and_then(|r| r.get("label")) != Some(&serde_json::json!("noadd"))
}
rustango::register_object_permission!("rowscope_child", "add", not_noadd);

/// An audited child under the same scope and `add` hook, so the audited
/// write arms run the after-write check too.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rowscope_achild", audit(track = "label"))]
#[allow(dead_code)]
pub struct AuditedChild {
    #[rustango(primary_key, max_length = 16)]
    pub code: String,
    pub parent_id: i64,
    #[rustango(max_length = 64)]
    pub label: String,
}

rustango::register_admin_inline!(
    parent = "rowscope_parent",
    child = "rowscope_achild",
    fk = "parent_id",
    fields = &["code", "label"],
    extra = 1,
    max_num = Some(2),
);
rustango::register_queryset_scope!("rowscope_achild", not_out);
rustango::register_object_permission!("rowscope_achild", "add", not_noadd);

/// An auto-PK child with a secret, capped at one row per parent.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_autochild",
    admin(formfield_overrides = "token:password")
)]
#[allow(dead_code)]
pub struct AutoChild {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub parent_id: i64,
    #[rustango(max_length = 64)]
    pub note: String,
    #[rustango(max_length = 64)]
    pub token: String,
}

rustango::register_admin_inline!(
    parent = "rowscope_parent",
    child = "rowscope_autochild",
    fk = "parent_id",
    fields = &["note"],
    extra = 1,
    max_num = Some(1),
);

#[derive(Model, Debug, Clone)]
#[rustango(table = "rowscope_fparent", display = "name")]
#[allow(dead_code)]
pub struct FlagParent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

/// A NOT NULL and a nullable bool, edited top-level and inline (#1897).
#[derive(Model, Debug, Clone)]
#[rustango(table = "rowscope_flag")]
#[allow(dead_code)]
pub struct Flag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub parent_id: i64,
    pub active: bool,
    #[rustango(default = "false")]
    pub maybe: Option<bool>,
}

rustango::register_admin_inline!(
    parent = "rowscope_fparent",
    child = "rowscope_flag",
    fk = "parent_id",
    fields = &["active", "maybe"],
    extra = 3,
);

/// The view hook hides `hide*` titles; nothing on the list may count them.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_doc",
    admin(
        list_display = "title",
        list_filter = "kind",
        search_fields = "title",
        date_hierarchy = "filed_on",
        list_per_page = 1
    )
)]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(max_length = 16)]
    pub kind: String,
    pub filed_on: chrono::NaiveDate,
}

fn not_hide_title(_: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    row.and_then(|r| r["title"].as_str())
        .is_none_or(|t| !t.starts_with("hide"))
}
rustango::register_object_permission!("rowscope_doc", "view", not_hide_title);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    fresh_table::<Doc>(pool).await;
    drop_table(pool, "rowscope_note").await;
    fresh_table::<Item>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Parent>(pool).await;
    fresh_table::<Child>(pool).await;
    fresh_table::<AutoChild>(pool).await;
    fresh_table::<AuditedChild>(pool).await;
    fresh_table::<FlagParent>(pool).await;
    fresh_table::<Flag>(pool).await;
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
    rustango::audit::AuditLog::delete_where("entity_table", "rowscope_item", pool)
        .await
        .expect("clear audit rows");
}

async fn post(pool: &Pool, uri: &str, form: &str) -> (StatusCode, String) {
    post_as(pool, uri, form, |b| b).await
}

async fn post_as(
    pool: &Pool,
    uri: &str,
    form: &str,
    admin: impl FnOnce(rustango::admin::Builder) -> rustango::admin::Builder,
) -> (StatusCode, String) {
    let app = admin(rustango::admin::Builder::new(pool.clone()).admin_prefix("")).build();
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
    get_as(pool, uri, |b| b).await
}

async fn get_as(
    pool: &Pool,
    uri: &str,
    admin: impl FnOnce(rustango::admin::Builder) -> rustango::admin::Builder,
) -> (StatusCode, String) {
    let app = admin(rustango::admin::Builder::new(pool.clone()).admin_prefix("")).build();
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

async fn items(pool: &Pool) -> Vec<(String, i64)> {
    let mut rows = Item::objects().fetch(pool).await.expect("fetch items");
    rows.sort_by_key(|i| *i.id.get().expect("pk"));
    rows.into_iter().map(|i| (i.title, i.owner_id)).collect()
}

async fn item_audit_rows(pool: &Pool) -> i64 {
    rustango::audit::AuditLog::objects()
        .filter("entity_table", "rowscope_item")
        .count(pool)
        .await
        .expect("count audit rows")
}

/// A create outside the scope, or one the `add` hook refuses, writes nothing:
/// no row, no audit entry, no `post_save`.
async fn create_outside_scope_is_refused(pool: &Pool) {
    use rustango::signals::admin::{connect_admin_post_save, disconnect_admin_post_save};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let saves = Arc::new(AtomicUsize::new(0));
    let seen = saves.clone();
    let id = connect_admin_post_save(move |ctx| {
        // Creates only: edits in tests running alongside fire this global signal too.
        if ctx.table == "rowscope_item" && !ctx.change {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        async {}
    });
    let (status, body) = post(pool, "/rowscope_item", "title=t&owner_id=77").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = post(pool, "/rowscope_item", "title=locked&owner_id=1").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(items(pool).await.is_empty(), "row written");
    assert_eq!(
        item_audit_rows(pool).await,
        0,
        "audit entry for a refused create"
    );
    assert_eq!(
        saves.load(Ordering::SeqCst),
        0,
        "post_save on a refused create"
    );
    let (status, body) = post(pool, "/rowscope_item", "title=ok&owner_id=1").await;
    disconnect_admin_post_save(id);
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(items(pool).await, [("ok".to_owned(), 1)]);
    assert_eq!(item_audit_rows(pool).await, 1);
    assert_eq!(saves.load(Ordering::SeqCst), 1);
}

/// An edit that moves a row out of scope writes nothing.
async fn update_out_of_scope_is_refused(pool: &Pool) {
    let mine = seed(pool, "mine", 1).await;
    let uri = format!("/rowscope_item/{mine}");
    let (status, body) = post(pool, &uri, "title=moved&owner_id=77").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(items(pool).await, [("mine".to_owned(), 1)]);
    let (status, body) = post(pool, &uri, "title=renamed&owner_id=1").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(items(pool).await, [("renamed".to_owned(), 1)]);
}

/// The `change` hook judges the row before the edit, so a draft can be locked once.
async fn change_hook_sees_the_row_before_the_edit(pool: &Pool) {
    let draft = seed(pool, "draft", 1).await;
    let uri = format!("/rowscope_item/{draft}");
    let (status, body) = post(pool, &uri, "title=locked&owner_id=1").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(items(pool).await, [("locked".to_owned(), 1)]);
    let (status, body) = post(pool, &uri, "title=again&owner_id=1").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(items(pool).await, [("locked".to_owned(), 1)]);
}

async fn parent_names(pool: &Pool) -> Vec<String> {
    let rows = Parent::objects().fetch(pool).await.expect("fetch parents");
    rows.into_iter().map(|p| p.name).collect()
}

/// An inline row edited or added outside the child's scope, or refused by its
/// `add` hook, is a 403 and rolls the whole edit back.
async fn inline_child_outside_scope_is_refused(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "one", false).await;
    let uri = format!("/rowscope_parent/{p}");
    let head = "name=renamed&rowscope_child-0-code=a";
    for (slots, extra) in [
        ("1", "&rowscope_child-0-label=out"),
        ("2", "&rowscope_child-0-label=one&rowscope_child-1-code=n&rowscope_child-1-label=out&rowscope_child-1-secret=s"),
        ("2", "&rowscope_child-0-label=one&rowscope_child-1-code=n&rowscope_child-1-label=noadd&rowscope_child-1-secret=s"),
    ] {
        let form = format!(
            "{head}&rowscope_child-TOTAL_FORMS={slots}&rowscope_child-INITIAL_FORMS=1{extra}"
        );
        let (status, body) = post(pool, &uri, &form).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{extra}: {body}");
        assert_eq!(child(pool, "a").await.expect("a").label, "one", "{extra}");
        assert!(child(pool, "n").await.is_none(), "{extra}: row inserted");
        assert_eq!(parent_names(pool).await, ["p"], "{extra}: parent edit kept");
    }
    let form = format!(
        "{head}&rowscope_child-TOTAL_FORMS=2&rowscope_child-INITIAL_FORMS=1\
         &rowscope_child-0-label=two&rowscope_child-1-code=n&rowscope_child-1-label=fine&rowscope_child-1-secret=s"
    );
    let (status, body) = post(pool, &uri, &form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(child(pool, "a").await.expect("a").label, "two");
    assert_eq!(child(pool, "n").await.expect("n").label, "fine");
}

/// The same refusals for an audited child, and no audit row survives them.
async fn audited_inline_child_outside_scope_is_refused(pool: &Pool) {
    let p = seed_parent(pool).await;
    AuditedChild {
        code: "a".into(),
        parent_id: p,
        label: "one".into(),
    }
    .insert_pool(pool)
    .await
    .expect("insert child");
    let label = |code: &'static str| async move {
        AuditedChild::objects()
            .filter("code", code)
            .fetch(pool)
            .await
            .expect("fetch child")
            .into_iter()
            .next()
            .map(|c| c.label)
    };
    let audit_rows = || {
        rustango::audit::AuditLog::objects()
            .filter("entity_table", "rowscope_achild")
            .count(pool)
    };
    let before = audit_rows().await.expect("count audit rows");
    let uri = format!("/rowscope_parent/{p}");
    let head = "name=renamed&rowscope_achild-0-code=a";
    for (slots, extra) in [
        ("1", "&rowscope_achild-0-label=out"),
        (
            "2",
            "&rowscope_achild-0-label=one&rowscope_achild-1-code=n&rowscope_achild-1-label=out",
        ),
        (
            "2",
            "&rowscope_achild-0-label=one&rowscope_achild-1-code=n&rowscope_achild-1-label=noadd",
        ),
    ] {
        let form = format!(
            "{head}&rowscope_achild-TOTAL_FORMS={slots}&rowscope_achild-INITIAL_FORMS=1{extra}"
        );
        let (status, body) = post(pool, &uri, &form).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{extra}: {body}");
        assert_eq!(label("a").await.as_deref(), Some("one"), "{extra}");
        assert!(label("n").await.is_none(), "{extra}: row inserted");
        assert_eq!(parent_names(pool).await, ["p"], "{extra}: parent edit kept");
        assert_eq!(
            audit_rows().await.expect("count audit rows"),
            before,
            "{extra}: audit row kept"
        );
    }
    let form = format!(
        "{head}&rowscope_achild-TOTAL_FORMS=2&rowscope_achild-INITIAL_FORMS=1\
         &rowscope_achild-0-label=two&rowscope_achild-1-code=n&rowscope_achild-1-label=fine"
    );
    let (status, body) = post(pool, &uri, &form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(label("a").await.as_deref(), Some("two"));
    assert_eq!(label("n").await.as_deref(), Some("fine"));
}

/// Totals, pages, facets, date buckets and search all leave out the rows
/// the view hook denies.
async fn view_hook_rows_are_uncounted(pool: &Pool) {
    let day = |y| chrono::NaiveDate::from_ymd_opt(y, 5, 5).unwrap();
    for (title, kind, y) in [
        ("hide-1", "leaked", 2019),
        ("ok-1", "a", 2024),
        ("ok-2", "a", 2024),
    ] {
        let mut d = Doc {
            id: Auto::default(),
            title: title.into(),
            kind: kind.into(),
            filed_on: day(y),
        };
        d.insert_pool(pool).await.expect("insert doc");
    }
    let (status, body) = get(pool, "/rowscope_doc").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("2 rows"), "total: {body}");
    assert!(
        body.contains("ok-1"),
        "page 1 is the first visible row: {body}"
    );
    assert!(body.contains("kind=a"), "facet: {body}");
    assert!(!body.contains("leaked"), "denied facet value: {body}");
    assert!(!body.contains("2019"), "denied date bucket: {body}");
    // Every searched row is denied: the `IS NULL` filter matches none.
    let (_, body) = get(pool, "/rowscope_doc?q=hide").await;
    assert!(body.contains("0 rows"), "search oracle: {body}");

    // Past the scan cap the counts are withheld, never leaked.
    let (status, body) = get_as(pool, "/rowscope_doc", |b| b.view_hook_scan_cap(2)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("row count hidden"), "total: {body}");
    assert!(
        !body.contains("3 rows") && !body.contains("2 rows"),
        "a total leaked: {body}"
    );
    assert!(!body.contains("kind=a"), "facets kept: {body}");
    assert!(
        !body.contains(r#"<nav class="date-hierarchy""#),
        "date strip kept: {body}"
    );
    assert!(!body.contains("hide-1"), "denied row shown: {body}");
    // A search that narrows the scan under the cap counts again.
    let (_, body) = get_as(pool, "/rowscope_doc?q=ok-1", |b| b.view_hook_scan_cap(2)).await;
    assert!(body.contains("1 row"), "filtered scan: {body}");

    // Past the cap, "next" needs a following row the hook admits.
    let mut d = Doc {
        id: Auto::default(),
        title: "hide-2".into(),
        kind: "a".into(),
        filed_on: day(2024),
    };
    d.insert_pool(pool).await.expect("insert doc");
    let next = "next &rarr;</a>";
    let (_, body) = get_as(pool, "/rowscope_doc?page=2", |b| b.view_hook_scan_cap(2)).await;
    assert!(body.contains(next), "an admitted next row: {body}");
    let (_, body) = get_as(pool, "/rowscope_doc?page=3", |b| b.view_hook_scan_cap(2)).await;
    assert!(body.contains("ok-2"), "page 3: {body}");
    assert!(!body.contains(next), "a denied next row leaked: {body}");
}

/// An FK facet labels a hidden target by its key, not its title (#2029).
async fn fk_facet_hides_a_hidden_targets_name(pool: &Pool) {
    let theirs = seed(pool, "secret-title", 77).await;
    // An id unlike the item's, so a JOIN-ON / WHERE bind swap shows.
    let mut note = Note {
        id: Auto::Set(7),
        body: "n".into(),
        item_id: theirs,
    };
    note.insert_pool(pool).await.expect("insert note");
    let (status, body) = get(pool, "/rowscope_note").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(&format!("item_id={theirs}")), "facet: {body}");
    assert!(!body.contains("secret-title"), "hidden name leaked: {body}");
    // Nor in the FK cell or the detail page's joined display (#2080).
    let (status, body) = get(pool, &format!("/rowscope_note/{}", note.id.get().unwrap())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("secret-title"), "detail leaked: {body}");
}

/// A visible target's name still shows in the FK cell (#2080 control).
async fn fk_cell_shows_a_visible_targets_name(pool: &Pool) {
    let mine = seed(pool, "shown-title", 1).await;
    // An id unlike the item's, so a JOIN-ON / WHERE bind swap shows.
    let mut note = Note {
        id: Auto::Set(7),
        body: "n".into(),
        item_id: mine,
    };
    note.insert_pool(pool).await.expect("insert note");
    let (status, body) = get(pool, &format!("/rowscope_note/{}", note.id.get().unwrap())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("shown-title"), "detail: {body}");
}

/// List and inline View links percent-encode a string PK (#2079).
async fn view_links_encode_the_pk(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a/b?c", p, "l", false).await;
    for uri in [
        "/rowscope_child".to_owned(),
        format!("/rowscope_parent/{p}"),
    ] {
        let (status, body) = get(pool, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let linked = [r#"a%2Fb%3Fc">View</a>"#, r#"a%2Fb%3Fc">#"#];
        assert!(linked.iter().any(|l| body.contains(l)), "{uri}: {body}");
    }
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

/// A repeated DELETE slot frees one row, not two, under `max_num`.
async fn inline_duplicate_deletes_do_not_bypass_max_num(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "one", false).await;
    seed_child(pool, "b", p, "two", false).await;
    let form = "name=p&rowscope_child-TOTAL_FORMS=4&rowscope_child-INITIAL_FORMS=2\
        &rowscope_child-0-code=a&rowscope_child-0-DELETE=on\
        &rowscope_child-1-code=a&rowscope_child-1-DELETE=on\
        &rowscope_child-2-code=c&rowscope_child-2-label=c&rowscope_child-2-secret=s\
        &rowscope_child-3-code=d&rowscope_child-3-label=d&rowscope_child-3-secret=s";
    let (status, body) = post(pool, &format!("/rowscope_parent/{p}"), form).await;
    assert!(body.contains("at most 2 rows"), "{status}: {body}");
    assert!(
        child(pool, "d").await.is_none(),
        "row past max_num was written"
    );
}

/// A read-only user guessing a secret gets the same answer right or wrong.
async fn inline_secret_guess_is_no_oracle(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "one", false).await;
    let mut statuses = Vec::new();
    for guess in ["pw-a", "wrong"] {
        let form = format!(
            "name=p&rowscope_child-TOTAL_FORMS=1&rowscope_child-INITIAL_FORMS=1\
             &rowscope_child-0-code=a&rowscope_child-0-label=one&rowscope_child-0-secret={guess}"
        );
        let uri = format!("/rowscope_parent/{p}");
        let (status, _) = post_as(pool, &uri, &form, |b| b.read_only(["rowscope_child"])).await;
        statuses.push(status);
    }
    assert_eq!(statuses[0], statuses[1], "the answer tells a right guess");
    assert_eq!(statuses[0], StatusCode::FORBIDDEN);
}

/// Editing or deleting a child the view hook refuses, by a guessed PK, is refused.
async fn inline_post_refuses_hidden_children(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "b", p, "ghost", true).await;
    for slot in [
        "rowscope_child-0-label=edited",
        "rowscope_child-0-DELETE=on",
    ] {
        let form = format!(
            "name=p&rowscope_child-TOTAL_FORMS=1&rowscope_child-INITIAL_FORMS=1\
             &rowscope_child-0-code=b&{slot}"
        );
        let (status, body) = post(pool, &format!("/rowscope_parent/{p}"), &form).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{slot}: {body}");
        let b = child(pool, "b").await.expect("hidden row kept");
        assert_eq!(b.label, "ghost", "{slot}");
    }
}

/// `max_num` counts the rows the queryset hook hides too.
async fn inline_max_num_counts_hidden_rows(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "out", false).await;
    seed_child(pool, "b", p, "two", false).await;
    let form = "name=p&rowscope_child-TOTAL_FORMS=2&rowscope_child-INITIAL_FORMS=1\
        &rowscope_child-0-code=b&rowscope_child-0-label=two\
        &rowscope_child-1-code=c&rowscope_child-1-label=c&rowscope_child-1-secret=s";
    let (status, body) = post(pool, &format!("/rowscope_parent/{p}"), form).await;
    assert!(body.contains("at most 2 rows"), "{status}: {body}");
    assert!(
        child(pool, "c").await.is_none(),
        "row past max_num was written"
    );
}

async fn seed_auto_child(pool: &Pool, parent_id: i64, note: &str) -> i64 {
    let mut c = AutoChild {
        id: Auto::default(),
        parent_id,
        note: note.into(),
        token: "tok".into(),
    };
    c.insert_pool(pool).await.expect("insert auto child");
    *c.id.get().expect("pk")
}

async fn auto_children(pool: &Pool) -> Vec<AutoChild> {
    AutoChild::objects().fetch(pool).await.expect("fetch")
}

/// A lowered INITIAL_FORMS turns an auto-PK row into an insert: it needs `add` and `max_num`.
async fn inline_lowered_initial_forms_is_an_insert(pool: &Pool) {
    let p = seed_parent(pool).await;
    let id = seed_auto_child(pool, p, "old").await;
    let form = format!(
        "name=p&rowscope_autochild-TOTAL_FORMS=1&rowscope_autochild-INITIAL_FORMS=0\
         &rowscope_autochild-0-id={id}&rowscope_autochild-0-note=dup"
    );
    let uri = format!("/rowscope_parent/{p}");
    let perms = [
        "rowscope_parent.view",
        "rowscope_parent.change",
        "rowscope_autochild.view",
        "rowscope_autochild.change",
    ];
    let (status, body) = post_as(pool, &uri, &form, |b| {
        b.with_user_perms(perms.iter().map(|p| (*p).to_owned()))
    })
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no add perm: {body}");
    let (status, body) = post(pool, &uri, &form).await;
    assert!(body.contains("at most 1 rows"), "{status}: {body}");
    let rows = auto_children(pool).await;
    assert_eq!(rows.len(), 1, "row inserted");
    assert_eq!(rows[0].note, "old", "row updated");
}

/// A row another save commits after the plan counted still counts under `max_num`.
async fn inline_max_num_recounts_in_the_save_tx(pool: &Pool) {
    use rustango::signals::admin as sig;
    let p = seed_parent(pool).await;
    seed_child(pool, "a", p, "one", false).await;
    // Runs after the plan counted, before the save tx: a racing save's row.
    let racer = pool.clone();
    let id = sig::connect_admin_pre_save(move |ctx| {
        let racer = racer.clone();
        async move {
            if ctx.table == "rowscope_child" && !ctx.change {
                seed_child(&racer, "r", p, "racer", false).await;
            }
        }
    });
    let form = "name=p&rowscope_child-TOTAL_FORMS=2&rowscope_child-INITIAL_FORMS=1\
        &rowscope_child-0-code=a&rowscope_child-0-label=one\
        &rowscope_child-1-code=c&rowscope_child-1-label=mine&rowscope_child-1-secret=s";
    let (status, body) = post(pool, &format!("/rowscope_parent/{p}"), form).await;
    sig::disconnect_admin_pre_save(id);
    assert!(body.contains("allows no more rows"), "{status}: {body}");
    assert!(
        child(pool, "c").await.is_none(),
        "row past max_num was written"
    );
}

/// `?<secret>__isnull=1` is ignored, so it cannot probe whether a secret is set.
async fn list_ignores_isnull_on_a_secret(pool: &Pool) {
    let p = seed_parent(pool).await;
    seed_auto_child(pool, p, "listed-note").await;
    let (status, body) = get(pool, "/rowscope_autochild?token__isnull=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("listed-note"),
        "secret filter applied: {body}"
    );
}

async fn flags(pool: &Pool) -> Vec<(bool, Option<bool>)> {
    let mut rows = Flag::objects().fetch(pool).await.expect("fetch flags");
    rows.sort_by_key(|f| *f.id.get().expect("pk"));
    rows.iter().map(|f| (f.active, f.maybe)).collect()
}

/// "" saves NULL, "false" saves false, an unticked NOT NULL box saves false.
async fn nullable_bool_round_trips_in_admin_form(pool: &Pool) {
    let (status, body) = get(pool, "/rowscope_flag/new").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains(r#"value="false" selected"#),
        "default No: {body}"
    );

    let (status, body) = post(pool, "/rowscope_flag", "parent_id=1&maybe=").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(flags(pool).await, [(false, None)]);

    let id = *Flag::objects().fetch(pool).await.expect("fetch")[0]
        .id
        .get()
        .expect("pk");
    let uri = format!("/rowscope_flag/{id}");
    let (status, body) = post(pool, &uri, "parent_id=1&active=true&maybe=false").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(flags(pool).await, [(true, Some(false))]);

    let (status, body) = post(pool, &uri, "parent_id=1&maybe=").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(flags(pool).await, [(false, None)]);

    // An absent nullable key is NULL too, not false.
    post(pool, &uri, "parent_id=1&active=true&maybe=true").await;
    let (status, body) = post(pool, &uri, "parent_id=1&active=true").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(flags(pool).await, [(true, None)]);
}

async fn nullable_bool_round_trips_in_inline(pool: &Pool) {
    let mut p = FlagParent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("insert parent");
    let p = *p.id.get().expect("pk");
    let form = "name=p&rowscope_flag-TOTAL_FORMS=3&rowscope_flag-INITIAL_FORMS=0\
        &rowscope_flag-0-active=true&rowscope_flag-0-maybe=\
        &rowscope_flag-1-maybe=false&rowscope_flag-2-active=true";
    let (status, body) = post(pool, &format!("/rowscope_fparent/{p}"), form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(
        flags(pool).await,
        [(true, None), (false, Some(false)), (true, None)]
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        nullable_bool_round_trips_in_admin_form,
        nullable_bool_round_trips_in_inline,
        hidden_rows_are_404_and_uncounted,
        view_hook_rows_are_uncounted,
        create_outside_scope_is_refused,
        update_out_of_scope_is_refused,
        change_hook_sees_the_row_before_the_edit,
        inline_child_outside_scope_is_refused,
        audited_inline_child_outside_scope_is_refused,
        fk_facet_hides_a_hidden_targets_name,
        fk_cell_shows_a_visible_targets_name,
        view_links_encode_the_pk,
        inlines_hide_secrets_and_refused_rows,
        inline_post_keeps_secrets_and_inserts_natural_pks,
        inline_post_enforces_max_num,
        inline_duplicate_deletes_do_not_bypass_max_num,
        inline_secret_guess_is_no_oracle,
        inline_post_refuses_hidden_children,
        inline_max_num_counts_hidden_rows,
        inline_lowered_initial_forms_is_an_insert,
        inline_max_num_recounts_in_the_save_tx,
        list_ignores_isnull_on_a_secret,
    ],
}
