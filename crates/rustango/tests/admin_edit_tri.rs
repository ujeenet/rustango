//! Admin edit forms on every backend: an untouched save keeps what was
//! stored; inline rows take no auto or generated input.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub meta: Option<serde_json::Value>,
    #[rustango(max_length = 16, choices = "draft:Draft, live:Live")]
    pub status: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_order")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_line")]
#[allow(dead_code)]
pub struct Line {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aed_order", on = "id")]
    pub order_id: i64,
    pub qty: i64,
    #[rustango(generated_as = "qty * 2")]
    pub twice: i64,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

rustango::register_admin_inline!(
    parent = "aed_order",
    child = "aed_line",
    fk = "order_id",
    extra = 1,
);

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_sorder")]
#[allow(dead_code)]
pub struct SOrder {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_sline")]
#[allow(dead_code)]
pub struct SLine {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aed_sorder", on = "id")]
    pub order_id: i64,
    pub qty: i64,
}

rustango::register_admin_inline!(
    parent = "aed_sorder",
    child = "aed_sline",
    fk = "order_id",
    fields = &["qty"],
);

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_cparent")]
#[allow(dead_code)]
pub struct CParent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 8)]
    pub name: String,
}

/// An inline child whose key and shown field have their own column names.
#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_cline")]
#[allow(dead_code)]
pub struct CLine {
    #[rustango(primary_key, column = "line_pk")]
    pub id: Auto<i64>,
    #[rustango(fk = "aed_cparent", on = "id")]
    pub parent_id: i64,
    #[rustango(column = "amt")]
    pub amount: i64,
}

rustango::register_admin_inline!(
    parent = "aed_cparent",
    child = "aed_cline",
    fk = "parent_id",
    fields = &["amount"],
);

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_slug")]
#[allow(dead_code)]
pub struct Slugged {
    #[rustango(primary_key, max_length = 64)]
    pub slug: String,
    #[rustango(max_length = 32)]
    pub name: Option<String>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_slugref")]
#[allow(dead_code)]
pub struct SlugRef {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aed_slug", on = "slug", max_length = 64)]
    pub slug_id: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_dparent")]
#[allow(dead_code)]
pub struct DParent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 8)]
    pub name: String,
}

/// An inline child with a defaulted field and a sub-millisecond time.
#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_dline")]
#[allow(dead_code)]
pub struct DLine {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aed_dparent", on = "id")]
    pub parent_id: i64,
    pub qty: i64,
    #[rustango(max_length = 16, default = "'draft'")]
    pub status: String,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
}

rustango::register_admin_inline!(
    parent = "aed_dparent",
    child = "aed_dline",
    fk = "parent_id",
    fields = &["qty", "status", "at"],
);

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_rparent")]
#[allow(dead_code)]
pub struct RParent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub rank: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_rline")]
#[allow(dead_code)]
pub struct RLine {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aed_rparent", on = "id")]
    pub parent_id: i64,
    pub qty: i64,
    pub done: bool,
}

rustango::register_admin_inline!(
    parent = "aed_rparent",
    child = "aed_rline",
    fk = "parent_id",
    fields = &["qty", "done"],
    extra = 1,
);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "aed_slugref").await;
    fresh_table::<Slugged>(pool).await;
    fresh_table::<SlugRef>(pool).await;
    fresh_table::<Doc>(pool).await;
    drop_table(pool, "aed_line").await;
    drop_table(pool, "aed_sline").await;
    drop_table(pool, "aed_cline").await;
    fresh_table::<CParent>(pool).await;
    fresh_table::<CLine>(pool).await;
    fresh_table::<Order>(pool).await;
    fresh_table::<Line>(pool).await;
    fresh_table::<SOrder>(pool).await;
    fresh_table::<SLine>(pool).await;
    drop_table(pool, "aed_dline").await;
    fresh_table::<DParent>(pool).await;
    fresh_table::<DLine>(pool).await;
    drop_table(pool, "aed_rline").await;
    fresh_table::<RParent>(pool).await;
    fresh_table::<RLine>(pool).await;
}

/// The detail page lists the first 100 inline rows and 20 audit entries,
/// with links to the rest.
async fn detail_page_bounds_inline_rows_and_history(pool: &Pool) {
    let mut o = SOrder {
        id: Auto::default(),
        name: "o".into(),
    };
    o.insert_pool(pool).await.expect("order");
    let o = *o.id.get().expect("pk");
    for qty in 0..101 {
        let mut l = SLine {
            id: Auto::default(),
            order_id: o,
            qty,
        };
        l.insert_pool(pool).await.expect("line");
    }
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
    rustango::audit::AuditLog::delete_where("entity_table", "aed_sorder", pool)
        .await
        .expect("clear audit rows");
    for i in 0..21 {
        let mut e = rustango::audit::AuditLog {
            id: Auto::default(),
            entity_table: "aed_sorder".into(),
            entity_pk: o.to_string(),
            operation: "update".into(),
            source: "test".into(),
            changes: serde_json::json!({ "n": i }),
            occurred_at: chrono::Utc::now(),
        };
        e.insert_pool(pool).await.expect("audit row");
    }
    let (status, html) = send(pool, "GET", &format!("/aed_sorder/{o}"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert_eq!(
        html.matches(r#"href="/aed_sline/"#).count(),
        100,
        "inline rows"
    );
    assert!(
        html.contains(&format!(r#"href="/aed_sline?order_id={o}""#)),
        "no link to the other rows"
    );
    assert_eq!(
        html.matches(r#"class="audit-changes""#).count(),
        20,
        "audit rows"
    );
}

/// Queries one admin POST runs.
async fn post_counting(pool: &Pool, uri: &str, form: &str) -> (StatusCode, usize) {
    use rustango::test_assertions::QueryCounter;
    QueryCounter::scope(async {
        let (status, _) = send(pool, "POST", uri, form).await;
        (status, QueryCounter::current())
    })
    .await
}

/// Saving K unchanged inline rows reads them in one select, not K.
async fn inline_save_reads_children_in_one_select(pool: &Pool) {
    let mut o = SOrder {
        id: Auto::default(),
        name: "o".into(),
    };
    o.insert_pool(pool).await.expect("order");
    let o = *o.id.get().expect("pk");
    let mut form_rows = Vec::new();
    for qty in 0..10 {
        let mut l = SLine {
            id: Auto::default(),
            order_id: o,
            qty,
        };
        l.insert_pool(pool).await.expect("line");
        form_rows.push((*l.id.get().expect("pk"), qty));
    }
    let form = |rows: &[(i64, i64)]| {
        let mut f = format!(
            "name=o&aed_sline-TOTAL_FORMS={n}&aed_sline-INITIAL_FORMS={n}",
            n = rows.len()
        );
        for (i, (id, qty)) in rows.iter().enumerate() {
            f.push_str(&format!("&aed_sline-{i}-id={id}&aed_sline-{i}-qty={qty}"));
        }
        f
    };
    let uri = format!("/aed_sorder/{o}");
    let (s1, one) = post_counting(pool, &uri, &form(&form_rows[..1])).await;
    let (s10, ten) = post_counting(pool, &uri, &form(&form_rows)).await;
    assert!(s1.is_redirection() && s10.is_redirection(), "{s1} {s10}");
    assert_eq!(one, ten, "child reads scale with the rows");
}

/// Signal receivers are process-global; the SQLite arm takes no live lock.
fn signal_lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Inline child writes send one admin signal per row, pre and post.
async fn inline_child_writes_send_admin_signals(pool: &Pool) {
    use rustango::signals::admin as sig;
    use std::sync::{Arc, Mutex};
    let mut o = SOrder {
        id: Auto::default(),
        name: "o".into(),
    };
    o.insert_pool(pool).await.expect("order");
    let o = *o.id.get().expect("pk");
    let mut ids = Vec::new();
    for qty in [1, 2] {
        let mut l = SLine {
            id: Auto::default(),
            order_id: o,
            qty,
        };
        l.insert_pool(pool).await.expect("line");
        ids.push(*l.id.get().expect("pk"));
    }
    let _guard = signal_lock().lock().await;
    sig::clear_all();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = |tag: &'static str| {
        let seen = seen.clone();
        move |table: &'static str, pk: String, extra: &str| {
            if table == "aed_sline" {
                seen.lock().unwrap().push(format!("{tag} {pk}{extra}"));
            }
        }
    };
    let (a, b, c, d) = (
        log("pre_save"),
        log("post_save"),
        log("pre_delete"),
        log("post_delete"),
    );
    sig::connect_admin_pre_save(move |x| {
        a(x.table, x.pk, if x.change { " change" } else { " add" });
        async {}
    });
    sig::connect_admin_post_save(move |x| {
        b(x.table, x.pk, if x.change { " change" } else { " add" });
        async {}
    });
    sig::connect_admin_pre_delete(move |x| {
        c(x.table, x.pk, "");
        async {}
    });
    sig::connect_admin_post_delete(move |x| {
        d(x.table, x.pk, "");
        async {}
    });
    let form = format!(
        "name=o&aed_sline-TOTAL_FORMS=3&aed_sline-INITIAL_FORMS=2\
         &aed_sline-0-id={}&aed_sline-0-qty=5\
         &aed_sline-1-id={}&aed_sline-1-qty=2&aed_sline-1-DELETE=on\
         &aed_sline-2-qty=9",
        ids[0], ids[1]
    );
    let (status, body) = send(pool, "POST", &format!("/aed_sorder/{o}"), &form).await;
    sig::clear_all();
    assert!(status.is_redirection(), "{status}: {body}");
    let added = SLine::objects()
        .fetch(pool)
        .await
        .expect("lines")
        .into_iter()
        .find(|l| l.qty == 9)
        .expect("added row");
    let added = *added.id.get().expect("pk");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 6, "{seen:?}");
    // Every pre comes before every post: the posts fire after the commit.
    let sorted = |s: &[String]| {
        let mut s = s.to_vec();
        s.sort();
        s
    };
    let mut pre = vec![
        format!("pre_save {} change", ids[0]),
        format!("pre_delete {}", ids[1]),
        "pre_save  add".to_owned(),
    ];
    pre.sort();
    let mut post = vec![
        format!("post_save {} change", ids[0]),
        format!("post_delete {}", ids[1]),
        format!("post_save {added} add"),
    ];
    post.sort();
    assert_eq!(sorted(&seen[..3]), pre, "{seen:?}");
    assert_eq!(sorted(&seen[3..]), post, "{seen:?}");
}

async fn seed_order(pool: &Pool) -> i64 {
    let mut o = Order {
        id: Auto::default(),
        name: "o".into(),
    };
    o.insert_pool(pool).await.expect("order");
    *o.id.get().expect("pk")
}

async fn lines(pool: &Pool) -> Vec<Line> {
    Line::objects().fetch(pool).await.expect("lines")
}

/// Inline rows render no input for auto and generated fields, and a
/// row added or edited without them saves.
async fn inline_rows_take_no_auto_or_generated_input(pool: &Pool) {
    let o = seed_order(pool).await;
    let add = "name=o&aed_line-TOTAL_FORMS=1&aed_line-INITIAL_FORMS=0&aed_line-0-qty=3";
    let (status, body) = send(pool, "POST", &format!("/aed_order/{o}"), add).await;
    assert!(status.is_redirection(), "add: {status} {body}");
    let before = lines(pool).await.remove(0);
    let lid = *before.id.get().expect("pk");
    let stamp = *before.created_at.get().expect("stamped");

    let (status, html) = send(pool, "GET", &format!("/aed_order/{o}/edit"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    for f in ["created_at", "twice", "id"] {
        for i in 0..2 {
            let name = format!(r#"name="aed_line-{i}-{f}""#);
            let hidden = format!(r#"type="hidden" {name}"#);
            assert!(
                !html.contains(&name) || (f == "id" && html.contains(&hidden)),
                "input for {f} in row {i}"
            );
        }
    }

    let edit = format!(
        "name=o&aed_line-TOTAL_FORMS=1&aed_line-INITIAL_FORMS=1\
         &aed_line-0-id={lid}&aed_line-0-qty=5"
    );
    let (status, body) = send(pool, "POST", &format!("/aed_order/{o}"), &edit).await;
    assert!(status.is_redirection(), "edit: {status} {body}");
    let after = lines(pool).await.remove(0);
    assert_eq!(after.qty, 5);
    assert_eq!(after.twice, 10);
    assert_eq!(after.created_at.get(), Some(&stamp), "created_at rewritten");
}

async fn send(pool: &Pool, method: &str, uri: &str, form: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// What a browser submits for control `name` of the rendered form.
fn shown(html: &str, name: &str) -> String {
    let open = format!(r#"name="{name}""#);
    let at = html
        .find(&open)
        .unwrap_or_else(|| panic!("no `{name}` control"));
    let tag_start = html[..at].rfind('<').unwrap();
    let rest = &html[tag_start..];
    if rest.starts_with("<textarea") {
        let body = &rest[rest.find('>').unwrap() + 1..];
        return unescape(&body[..body.find("</textarea>").unwrap()]);
    }
    if rest.starts_with("<select") {
        let select = &rest[..rest.find("</select>").unwrap()];
        let options: Vec<&str> = select.split("<option").skip(1).collect();
        let pick = options
            .iter()
            .find(|o| o[..o.find('>').unwrap()].contains(" selected"))
            .or(options.first())
            .expect("an option");
        let v = &pick[pick.find("value=\"").unwrap() + 7..];
        return unescape(&v[..v.find('"').unwrap()]);
    }
    let tag = &rest[..rest.find('>').unwrap()];
    let v = &tag[tag.find("value=\"").unwrap() + 7..];
    unescape(&v[..v.find('"').unwrap()])
}

fn encode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Seed one doc, then rename it through the edit form as a browser would.
async fn rename_through_the_form(
    pool: &Pool,
    meta: Option<serde_json::Value>,
    legacy: bool,
) -> Doc {
    let mut d = Doc {
        id: Auto::default(),
        name: "a".into(),
        meta,
        status: "draft".into(),
    };
    d.insert_pool(pool).await.expect("insert");
    let id = *d.id.get().expect("pk");
    if legacy {
        // Written before `choices` dropped it; the ORM refuses it now.
        rustango::sql::raw_execute_pool(pool, "UPDATE aed_doc SET status = 'legacy'", Vec::new())
            .await
            .expect("legacy status");
    }
    let (status, html) = send(pool, "GET", &format!("/aed_doc/{id}/edit"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let form = encode(&[
        ("name", "renamed".to_owned()),
        ("meta", shown(&html, "meta")),
        ("status", shown(&html, "status")),
    ]);
    let (status, body) = send(pool, "POST", &format!("/aed_doc/{id}"), &form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let row = Doc::objects().fetch(pool).await.expect("fetch").remove(0);
    assert_eq!(row.name, "renamed");
    row
}

/// A nullable `{}` survives a save that only renames the row.
async fn untouched_nullable_json_object_is_kept(pool: &Pool) {
    let row = rename_through_the_form(pool, Some(serde_json::json!({})), false).await;
    assert_eq!(row.meta, Some(serde_json::json!({})), "nullable {{}} lost");
}

/// A stored value outside `choices` survives a rename.
async fn untouched_unknown_choice_is_kept(pool: &Pool) {
    let row = rename_through_the_form(pool, None, true).await;
    assert_eq!(row.status, "legacy", "unknown choice overwritten");
    assert_eq!(row.meta, None);
}

/// A plain field sent as shown still overwrites a change made after the
/// form loaded: only lossy prefills are left out of the UPDATE.
async fn stale_form_still_wins(pool: &Pool) {
    use rustango::signals::admin as sig;
    let mut d = Doc {
        id: Auto::default(),
        name: "a".into(),
        meta: None,
        status: "draft".into(),
    };
    d.insert_pool(pool).await.expect("insert");
    let id = *d.id.get().expect("pk");
    let (status, html) = send(pool, "GET", &format!("/aed_doc/{id}/edit"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let form = encode(&[
        ("name", shown(&html, "name")),
        ("meta", shown(&html, "meta")),
        ("status", shown(&html, "status")),
    ]);
    let _guard = signal_lock().lock().await;
    sig::clear_all();
    // Another writer renames the row between the form's read and its UPDATE.
    let other = pool.clone();
    sig::connect_admin_pre_save(move |x| {
        let other = other.clone();
        async move {
            if x.table == "aed_doc" {
                let mut row = Doc::objects().fetch(&other).await.expect("fetch").remove(0);
                row.name = "z".into();
                row.save_pool(&other).await.expect("concurrent edit");
            }
        }
    });
    let (status, body) = send(pool, "POST", &format!("/aed_doc/{id}"), &form).await;
    sig::clear_all();
    assert!(status.is_redirection(), "{status}: {body}");
    let row = Doc::objects().fetch(pool).await.expect("fetch").remove(0);
    assert_eq!(row.name, "a", "the submitted value lost to a later write");
}

/// Inline rows show a key and a field stored under other column names, and
/// an untouched save keeps the row instead of adding a copy.
async fn inline_rows_read_renamed_columns(pool: &Pool) {
    let mut p = CParent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("parent");
    let p = *p.id.get().expect("pk");
    let mut l = CLine {
        id: Auto::default(),
        parent_id: p,
        amount: 4242,
    };
    l.insert_pool(pool).await.expect("line");
    let l = *l.id.get().expect("pk");

    let (status, html) = send(pool, "GET", &format!("/aed_cparent/{p}"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains(&format!(r#"href="/aed_cline/{l}""#)),
        "{html}"
    );
    assert!(html.contains("4242"), "the shown field is blank: {html}");

    let (status, html) = send(pool, "GET", &format!("/aed_cparent/{p}/edit"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let key = shown(&html, "aed_cline-0-id");
    assert_eq!(key, l.to_string(), "the row's hidden key");
    let form = format!(
        "name=p&aed_cline-TOTAL_FORMS=1&aed_cline-INITIAL_FORMS=1&aed_cline-0-id={key}&aed_cline-0-amount=4242"
    );
    let (status, body) = send(pool, "POST", &format!("/aed_cparent/{p}"), &form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(CLine::objects().count(pool).await.expect("count"), 1);
}

/// Keys that a raw URL segment would misroute, split or reject.
const AWKWARD_SLUGS: &[&str] = &["a/b", "a?b", "a#b", "50%", "a b", "café ü"];

async fn seed_slugs(pool: &Pool) {
    for slug in std::iter::once("a").chain(AWKWARD_SLUGS.iter().copied()) {
        let row = Slugged {
            slug: slug.to_owned(),
            name: Some(format!("n-{slug}")),
        };
        row.insert_pool(pool).await.expect("insert slug");
    }
}

async fn slug_name(pool: &Pool, slug: &str) -> Option<String> {
    Slugged::objects()
        .filter("slug", slug.to_owned())
        .fetch(pool)
        .await
        .expect("fetch")
        .remove(0)
        .name
}

/// Detail, edit and delete links carry a string key as one encoded
/// segment, and following them reaches that row only.
async fn string_pk_links_are_encoded(pool: &Pool) {
    seed_slugs(pool).await;
    for slug in AWKWARD_SLUGS {
        let enc = urlencoding::encode(slug).into_owned();
        let (status, html) = send(pool, "GET", &format!("/aed_slug/{enc}"), "").await;
        assert_eq!(status, StatusCode::OK, "{slug}: {html}");
        for link in [
            format!(r#"href="/aed_slug/{enc}/edit""#),
            format!(r#"action="/aed_slug/{enc}/delete""#),
        ] {
            assert!(html.contains(&link), "{slug}: no {link}");
        }
        let (status, html) = send(pool, "GET", &format!("/aed_slug/{enc}/edit"), "").await;
        assert_eq!(status, StatusCode::OK, "{slug}: {html}");
        let html = html.replace("&#x2F;", "/");
        for link in [
            format!(r#"action="/aed_slug/{enc}""#),
            format!(r#"href="/aed_slug/{enc}">"#),
        ] {
            assert!(html.contains(&link), "{slug}: no {link}");
        }
        let form = encode(&[("name", format!("renamed-{slug}"))]);
        let (status, body) = send(pool, "POST", &format!("/aed_slug/{enc}"), &form).await;
        assert!(status.is_redirection(), "{slug}: {status}: {body}");
        assert_eq!(
            slug_name(pool, slug).await.as_deref(),
            Some(&*format!("renamed-{slug}"))
        );
    }
    assert_eq!(
        slug_name(pool, "a").await.as_deref(),
        Some("n-a"),
        "row `a` touched"
    );
}

/// A string FK cell on the list links to the target row's encoded URL.
async fn string_fk_cell_link_is_encoded(pool: &Pool) {
    seed_slugs(pool).await;
    for slug in AWKWARD_SLUGS {
        let mut r = SlugRef {
            id: Auto::default(),
            slug_id: (*slug).to_owned(),
        };
        r.insert_pool(pool).await.expect("insert ref");
    }
    let (status, html) = send(pool, "GET", "/aed_slugref", "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    for slug in AWKWARD_SLUGS {
        let enc = urlencoding::encode(slug).into_owned();
        let link = format!(r#"href="/aed_slug/{enc}""#);
        assert!(html.contains(&link), "{slug}: no {link} in {html}");
    }
}

/// A user without `{table}.change` opening an edit link lands on the
/// read-only detail page instead of a form whose save is refused.
async fn read_only_edit_link_shows_detail(pool: &Pool) {
    seed_slugs(pool).await;
    let enc = urlencoding::encode("a/b").into_owned();
    let builders = [
        rustango::admin::Builder::new(pool.clone()).with_user_perms(["aed_slug.view".to_owned()]),
        rustango::admin::Builder::new(pool.clone()).read_only(["aed_slug"]),
    ];
    for builder in builders {
        let app = builder.admin_prefix("").build();
        let req = Request::builder()
            .uri(format!("/aed_slug/{enc}/edit"))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert!(res.status().is_redirection(), "{}", res.status());
        let to = res.headers()["location"].to_str().unwrap().to_owned();
        assert_eq!(to, format!("/aed_slug/{enc}"));
    }
}

async fn seed_dparent(pool: &Pool) -> i64 {
    let mut p = DParent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("parent");
    *p.id.get().expect("pk")
}

/// An inline add row that does not send a defaulted field gets the column default.
async fn inline_add_row_takes_the_column_default(pool: &Pool) {
    let p = seed_dparent(pool).await;
    let add = "name=p&aed_dline-TOTAL_FORMS=1&aed_dline-INITIAL_FORMS=0&aed_dline-0-qty=3";
    let (status, body) = send(pool, "POST", &format!("/aed_dparent/{p}"), add).await;
    assert!(status.is_redirection(), "add: {status} {body}");
    let rows = DLine::objects().fetch(pool).await.expect("lines");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].qty, rows[0].status.as_str()), (3, "draft"));
}

/// Editing one field of an inline row keeps a time the input shows only to ms.
async fn inline_edit_keeps_microseconds(pool: &Pool) {
    let p = seed_dparent(pool).await;
    let at = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05.123456Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut line = DLine {
        id: Auto::default(),
        parent_id: p,
        qty: 1,
        status: "live".into(),
        at: Some(at),
    };
    line.insert_pool(pool).await.expect("line");
    let lid = *line.id.get().expect("pk");
    let (status, html) = send(pool, "GET", &format!("/aed_dparent/{p}/edit"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let form = encode(&[
        ("name", "p".into()),
        ("aed_dline-TOTAL_FORMS", "1".into()),
        ("aed_dline-INITIAL_FORMS", "1".into()),
        ("aed_dline-0-id", lid.to_string()),
        ("aed_dline-0-qty", "2".into()),
        ("aed_dline-0-status", shown(&html, "aed_dline-0-status")),
        ("aed_dline-0-at", shown(&html, "aed_dline-0-at")),
    ]);
    let (status, body) = send(pool, "POST", &format!("/aed_dparent/{p}"), &form).await;
    assert!(status.is_redirection(), "edit: {status} {body}");
    let after = DLine::objects().fetch(pool).await.expect("lines").remove(0);
    assert_eq!((after.qty, after.at), (2, Some(at)));
}

/// A refused save shows the form again with the typed inline rows.
async fn refused_edit_keeps_the_typed_inline_rows(pool: &Pool) {
    let mut p = RParent {
        id: Auto::default(),
        name: "p".into(),
        rank: 1,
    };
    p.insert_pool(pool).await.expect("parent");
    let pid = *p.id.get().expect("pk");
    let mut l = RLine {
        id: Auto::default(),
        parent_id: pid,
        qty: 1,
        done: false,
    };
    l.insert_pool(pool).await.expect("line");
    let lid = *l.id.get().expect("pk");
    let form = |rank: &str, new_qty: &str| {
        format!(
            "name=p&rank={rank}&aed_rline-TOTAL_FORMS=2&aed_rline-INITIAL_FORMS=1\
             &aed_rline-0-id={lid}&aed_rline-0-qty=77&aed_rline-0-done=true\
             &aed_rline-0-DELETE=on&aed_rline-1-qty={new_qty}"
        )
    };
    // A parent field error, then an inline row error.
    for (rank, new_qty) in [("abc", "5"), ("2", "abc")] {
        let (status, html) = send(
            pool,
            "POST",
            &format!("/aed_rparent/{pid}"),
            &form(rank, new_qty),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(shown(&html, "aed_rline-TOTAL_FORMS"), "2", "{html}");
        assert_eq!(shown(&html, "aed_rline-0-qty"), "77", "typed edit lost");
        assert_eq!(
            shown(&html, "aed_rline-1-qty"),
            new_qty,
            "typed new row lost"
        );
        for (name, what) in [
            ("aed_rline-0-done", "box"),
            ("aed_rline-0-DELETE", "delete box"),
        ] {
            let at = html.find(&format!(r#"name="{name}""#)).expect(name);
            let tag = &html[html[..at].rfind('<').unwrap()..];
            assert!(
                tag[..tag.find('>').unwrap()].contains(" checked"),
                "{what} unchecked"
            );
        }
        assert!(
            html.replace("&#x2F;", "/")
                .contains(&format!(r#"action="/aed_rparent/{pid}""#)),
            "form action"
        );
    }
    let stored = RLine::objects().fetch(pool).await.expect("lines");
    assert_eq!(stored.len(), 1, "a refused save wrote rows");
    assert_eq!(stored[0].qty, 1, "a refused save wrote the row");
}

/// A refused save keys the typed inline rows by PK: a row deleted meanwhile
/// shows as an error, and the others keep their own values.
async fn refused_edit_keys_typed_rows_by_pk(pool: &Pool) {
    let mut p = RParent {
        id: Auto::default(),
        name: "p".into(),
        rank: 1,
    };
    p.insert_pool(pool).await.expect("parent");
    let pid = *p.id.get().expect("pk");
    let mut lines = Vec::new();
    for qty in [1, 2, 3] {
        let mut l = RLine {
            id: Auto::default(),
            parent_id: pid,
            qty,
            done: false,
        };
        l.insert_pool(pool).await.expect("line");
        lines.push(l);
    }
    let [a, b, c] = [0, 1, 2].map(|i| *lines[i].id.get().expect("pk"));
    // Another user deletes b after the page loaded.
    lines[1].delete_pool(pool).await.expect("delete b");
    let form = format!(
        "name=p&rank=abc&aed_rline-TOTAL_FORMS=4&aed_rline-INITIAL_FORMS=3\
         &aed_rline-0-id={a}&aed_rline-0-qty=10\
         &aed_rline-1-id={b}&aed_rline-1-qty=20\
         &aed_rline-2-id={c}&aed_rline-2-qty=30\
         &aed_rline-3-qty=40"
    );
    let (status, html) = send(pool, "POST", &format!("/aed_rparent/{pid}"), &form).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert_eq!(shown(&html, "aed_rline-INITIAL_FORMS"), "2", "{html}");
    assert_eq!(shown(&html, "aed_rline-TOTAL_FORMS"), "3", "{html}");
    let rows = [
        (
            shown(&html, "aed_rline-0-id"),
            shown(&html, "aed_rline-0-qty"),
        ),
        (
            shown(&html, "aed_rline-1-id"),
            shown(&html, "aed_rline-1-qty"),
        ),
    ];
    assert_eq!(
        rows,
        [(a.to_string(), "10".into()), (c.to_string(), "30".into())],
        "typed values moved to another row"
    );
    assert_eq!(shown(&html, "aed_rline-2-qty"), "40", "typed new row lost");
    assert!(!html.contains("aed_rline-3-"), "an extra slot: {html}");
    assert!(
        html.contains(&format!("Row {b} is not in this list")),
        "the gone row is not flagged: {html}"
    );

    // Save the form as shown, parent fixed: each row keeps its own data.
    let resubmit = encode(&[
        ("name", "p".into()),
        ("rank", "2".into()),
        ("aed_rline-TOTAL_FORMS", "3".into()),
        ("aed_rline-INITIAL_FORMS", "2".into()),
        ("aed_rline-0-id", rows[0].0.clone()),
        ("aed_rline-0-qty", rows[0].1.clone()),
        ("aed_rline-1-id", rows[1].0.clone()),
        ("aed_rline-1-qty", rows[1].1.clone()),
        ("aed_rline-2-qty", "40".into()),
    ]);
    let (status, body) = send(pool, "POST", &format!("/aed_rparent/{pid}"), &resubmit).await;
    assert!(status.is_redirection(), "save: {status} {body}");
    let mut stored: Vec<(i64, i64)> = RLine::objects()
        .fetch(pool)
        .await
        .expect("lines")
        .into_iter()
        .map(|l| (*l.id.get().expect("pk"), l.qty))
        .collect();
    stored.sort_unstable();
    assert_eq!(&stored[..2], [(a, 10), (c, 30)]);
    assert_eq!((stored.len(), stored[2].1), (3, 40));
}

/// The bare admin serves the favicon its pages link, outside the session gate.
async fn bare_admin_serves_the_favicon_it_links(pool: &Pool) {
    let admin = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("/admin")
        .build();
    let app = axum::Router::new().nest("/admin", admin);
    let get = |uri: String| {
        let app = app.clone();
        async move {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let kind = res
                .headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap().to_owned());
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (status, kind, String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    let (status, _, html) = get("/admin/aed_doc".into()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let at = html.find(r#"rel="icon""#).expect("a favicon link");
    let href = &html[at..];
    let href = &href[href.find("href=\"").unwrap() + 6..];
    let href = href[..href.find('"').unwrap()].replace("&#x2F;", "/");
    assert_eq!(href, "/admin/__static__/icon.png");
    let (status, kind, _) = get(href).await;
    assert_eq!(status, StatusCode::OK, "favicon");
    assert_eq!(kind.as_deref(), Some("image/png"));
    // Served before the session gate.
    let gated = rustango::admin::Builder::new(pool.clone())
        .with_session_auth(rustango::session::SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    let req = Request::builder()
        .uri("/__admin/__static__/icon.png")
        .body(Body::empty())
        .unwrap();
    let res = axum::Router::new()
        .nest("/__admin", gated)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "gated favicon");
}

/// Under a nested prefix, Home and the first breadcrumb open the index:
/// axum's `nest` serves `/admin`, and `/admin/` is 404.
async fn home_links_open_the_nested_index(pool: &Pool) {
    let admin = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("/admin")
        .build();
    let app = axum::Router::new().nest("/admin", admin);
    let get = |uri: String| {
        let app = app.clone();
        async move {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    for page in ["/admin/aed_doc", "/admin/aed_doc/new"] {
        let (status, html) = get(page.into()).await;
        assert_eq!(status, StatusCode::OK, "{page}: {html}");
        let html = html.replace("&#x2F;", "/");
        assert!(!html.contains(r#"href="/admin/""#), "{page} links /admin/");
        let homes = html.matches(r#"href="/admin""#).count();
        assert!(homes >= 2, "{page}: sidebar and breadcrumb, got {homes}");
    }
    let (status, html) = get("/admin".into()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        inline_add_row_takes_the_column_default,
        inline_edit_keeps_microseconds,
        string_pk_links_are_encoded,
        string_fk_cell_link_is_encoded,
        read_only_edit_link_shows_detail,
        inline_rows_read_renamed_columns,
        untouched_nullable_json_object_is_kept,
        untouched_unknown_choice_is_kept,
        stale_form_still_wins,
        inline_rows_take_no_auto_or_generated_input,
        inline_child_writes_send_admin_signals,
        detail_page_bounds_inline_rows_and_history,
        inline_save_reads_children_in_one_select,
        refused_edit_keeps_the_typed_inline_rows,
        refused_edit_keys_typed_rows_by_pk,
        bare_admin_serves_the_favicon_it_links,
        home_links_open_the_nested_index,
    ],
}

#[cfg(feature = "sqlite")]
mod sqlite_only {
    use super::*;

    /// A UUID key the column default writes as TEXT, not a bound BLOB.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "aed_textkey", admin(actions = "delete_selected"))]
    #[allow(dead_code)]
    pub struct TextKey {
        #[rustango(primary_key, auto_uuid, default = "(lower(hex(randomblob(16))))")]
        pub id: Auto<uuid::Uuid>,
        #[rustango(max_length = 32)]
        pub name: String,
    }

    /// The admin finds a TEXT-stored UUID key on detail, edit and delete.
    #[tokio::test]
    async fn text_uuid_key_is_found_by_change_and_delete() {
        use rustango::testkit::matrix::{live_lock, Backend};
        let _guard = live_lock().lock().await;
        let pool = Backend::Sqlite.pool().await.expect("sqlite pool");
        rustango::testkit::matrix::fresh_table::<TextKey>(&pool).await;
        let (status, body) = send(&pool, "POST", "/aed_textkey", "name=a").await;
        assert!(status.is_redirection(), "create: {status} {body}");
        let column = |c: &'static str| TextKey::objects().values_list_flat(c);
        let key: String = column("id")
            .fetch::<String>(&pool)
            .await
            .expect("read key")
            .remove(0);
        assert_eq!(key.len(), 32, "the default stores text: {key}");

        let (status, html) = send(&pool, "GET", &format!("/aed_textkey/{key}"), "").await;
        assert_eq!(status, StatusCode::OK, "detail: {html}");
        let (status, html) = send(&pool, "GET", &format!("/aed_textkey/{key}/edit"), "").await;
        assert_eq!(status, StatusCode::OK, "edit form: {html}");
        let (status, body) = send(&pool, "POST", &format!("/aed_textkey/{key}"), "name=b").await;
        assert!(status.is_redirection(), "edit: {status} {body}");
        let names = column("name")
            .fetch::<String>(&pool)
            .await
            .expect("read name");
        assert_eq!(names, ["b"], "the edit did not reach the row");
        let (status, body) = send(&pool, "POST", &format!("/aed_textkey/{key}/delete"), "").await;
        assert!(status.is_redirection(), "delete: {status} {body}");
        let left = column("id")
            .fetch::<String>(&pool)
            .await
            .expect("read keys");
        assert!(left.is_empty(), "the row was not deleted");
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "aed_tparent")]
    #[allow(dead_code)]
    pub struct TParent {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 32)]
        pub name: String,
    }

    /// An inline child whose UUID key the column default writes as TEXT.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "aed_tline")]
    #[allow(dead_code)]
    pub struct TLine {
        #[rustango(primary_key, auto_uuid, default = "(lower(hex(randomblob(16))))")]
        pub id: Auto<uuid::Uuid>,
        #[rustango(fk = "aed_tparent", on = "id")]
        pub parent_id: i64,
        pub qty: i64,
    }

    rustango::register_admin_inline!(
        parent = "aed_tparent",
        child = "aed_tline",
        fk = "parent_id",
        fields = &["qty"],
    );

    /// `delete_selected` and an inline edit find a TEXT-stored UUID key.
    #[tokio::test]
    async fn text_uuid_key_is_found_by_bulk_delete_and_inline_edit() {
        use rustango::testkit::matrix::{drop_table, fresh_table, live_lock, Backend};
        let _guard = live_lock().lock().await;
        let pool = Backend::Sqlite.pool().await.expect("sqlite pool");
        fresh_table::<TextKey>(&pool).await;
        drop_table(&pool, "aed_tline").await;
        fresh_table::<TParent>(&pool).await;
        fresh_table::<TLine>(&pool).await;

        let (status, body) = send(&pool, "POST", "/aed_textkey", "name=a").await;
        assert!(status.is_redirection(), "create: {status} {body}");
        let keys = |pool: &Pool| {
            let pool = pool.clone();
            async move {
                TextKey::objects()
                    .values_list_flat("id")
                    .fetch::<String>(&pool)
                    .await
                    .expect("read keys")
            }
        };
        let key = keys(&pool).await.remove(0);
        assert_eq!(key.len(), 32, "the default stores text: {key}");
        let form = format!("action=delete_selected&_selected={key}");
        let (status, body) = send(&pool, "POST", "/aed_textkey/__action", &form).await;
        assert!(status.is_redirection(), "bulk delete: {status} {body}");
        assert!(
            keys(&pool).await.is_empty(),
            "delete_selected missed the row"
        );

        let mut parent = TParent {
            id: Auto::default(),
            name: "p".into(),
        };
        parent.insert_pool(&pool).await.expect("parent");
        let pid = *parent.id.get().expect("pk");
        let add = "name=p&aed_tline-TOTAL_FORMS=1&aed_tline-INITIAL_FORMS=0&aed_tline-0-qty=3";
        let (status, body) = send(&pool, "POST", &format!("/aed_tparent/{pid}"), add).await;
        assert!(status.is_redirection(), "inline add: {status} {body}");
        // The admin binds a new key itself; make it the TEXT a default writes.
        let lines = TLine::objects()
            .values_list_flat("id")
            .fetch::<uuid::Uuid>(&pool)
            .await
            .expect("line key");
        let lid = lines[0].simple().to_string();
        rustango::sql::raw_execute_pool(
            &pool,
            "UPDATE aed_tline SET id = ?",
            vec![rustango::core::SqlValue::String(lid.clone())],
        )
        .await
        .expect("store the key as text");
        let edit = format!(
            "name=p&aed_tline-TOTAL_FORMS=1&aed_tline-INITIAL_FORMS=1\
             &aed_tline-0-id={lid}&aed_tline-0-qty=5"
        );
        let (status, body) = send(&pool, "POST", &format!("/aed_tparent/{pid}"), &edit).await;
        assert!(status.is_redirection(), "inline edit: {status} {body}");
        let qty = TLine::objects()
            .values_list_flat("qty")
            .fetch::<i64>(&pool)
            .await
            .expect("read qty");
        assert_eq!(qty, [5], "the inline edit did not reach the row");
    }
}

#[cfg(feature = "postgres")]
mod pg_only {
    use super::*;
    use rustango::sql::{Array, HStore, Range, Vector};

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "aed_pgkinds")]
    #[allow(dead_code)]
    pub struct PgKinds {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 32)]
        pub name: String,
        pub tags: Array<String>,
        pub seats: Range<i32>,
        pub attrs: HStore,
        #[rustango(vector(dims = 3))]
        pub emb: Vector,
        #[rustango(vector(dims = 3))]
        pub opt_emb: Option<Vector>,
    }

    /// Array, range, hstore and vector columns show their value and
    /// survive an untouched save.
    #[tokio::test]
    async fn untouched_pg_types_are_kept() {
        use rustango::testkit::matrix::{live_lock, Backend};
        let _guard = live_lock().lock().await;
        let Some(pool) = Backend::Postgres.pool().await else {
            eprintln!("DATABASE_URL not set — skipping");
            return;
        };
        for ext in ["hstore", "vector"] {
            rustango::sql::raw_execute_pool(
                &pool,
                &format!("CREATE EXTENSION IF NOT EXISTS {ext}"),
                Vec::new(),
            )
            .await
            .expect("extension");
        }
        rustango::testkit::matrix::fresh_table::<PgKinds>(&pool).await;
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("k".to_owned(), Some("v".to_owned()));
        let mut row = PgKinds {
            id: Auto::default(),
            name: "a".into(),
            tags: Array(vec!["x".into(), "y".into()]),
            seats: Range::closed_open(1, 10),
            attrs: HStore(attrs.clone()),
            emb: Vector::new(vec![1.0, 2.0, 3.0]),
            opt_emb: Some(Vector::new(vec![0.5, 0.0, 0.0])),
        };
        row.insert_pool(&pool).await.expect("insert");
        let id = *row.id.get().expect("pk");
        let (status, html) = send(&pool, "GET", &format!("/aed_pgkinds/{id}/edit"), "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let mut pairs = vec![("name", "renamed".to_owned())];
        for f in ["tags", "seats", "attrs", "emb", "opt_emb"] {
            pairs.push((f, shown(&html, f)));
        }
        assert_eq!(pairs[1].1, "x, y", "array prefill");
        let (status, body) = send(
            &pool,
            "POST",
            &format!("/aed_pgkinds/{id}"),
            &encode(&pairs),
        )
        .await;
        assert!(status.is_redirection(), "{status}: {body}");
        let got = PgKinds::objects()
            .fetch(&pool)
            .await
            .expect("fetch")
            .remove(0);
        assert_eq!(got.name, "renamed");
        assert_eq!(got.tags.0, ["x", "y"]);
        assert_eq!(got.seats, Range::closed_open(1, 10));
        assert_eq!(got.attrs.0, attrs);
        assert_eq!(got.emb, Vector::new(vec![1.0, 2.0, 3.0]));
        assert_eq!(got.opt_emb, Some(Vector::new(vec![0.5, 0.0, 0.0])));
    }
}
