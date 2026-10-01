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

fn not_out(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("label", Op::Ne, SqlValue::String("out".into()))]
}
rustango::register_admin_queryset!("rowscope_child", not_out);

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
    extra = 2,
);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "rowscope_note").await;
    fresh_table::<Item>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Parent>(pool).await;
    fresh_table::<Child>(pool).await;
    fresh_table::<AutoChild>(pool).await;
    fresh_table::<FlagParent>(pool).await;
    fresh_table::<Flag>(pool).await;
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
}

async fn nullable_bool_round_trips_in_inline(pool: &Pool) {
    let mut p = FlagParent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("insert parent");
    let p = *p.id.get().expect("pk");
    let form = "name=p&rowscope_flag-TOTAL_FORMS=2&rowscope_flag-INITIAL_FORMS=0\
        &rowscope_flag-0-active=true&rowscope_flag-0-maybe=\
        &rowscope_flag-1-maybe=false";
    let (status, body) = post(pool, &format!("/rowscope_fparent/{p}"), form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(flags(pool).await, [(true, None), (false, Some(false))]);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        nullable_bool_round_trips_in_admin_form,
        nullable_bool_round_trips_in_inline,
        hidden_rows_are_404_and_uncounted,
        fk_facet_hides_a_hidden_targets_name,
        inlines_hide_secrets_and_refused_rows,
        inline_post_keeps_secrets_and_inserts_natural_pks,
        inline_post_enforces_max_num,
        inline_duplicate_deletes_do_not_bypass_max_num,
        inline_secret_guess_is_no_oracle,
        inline_post_refuses_hidden_children,
        inline_max_num_counts_hidden_rows,
        inline_lowered_initial_forms_is_an_insert,
        list_ignores_isnull_on_a_secret,
    ],
}
