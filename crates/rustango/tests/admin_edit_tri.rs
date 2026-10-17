//! Admin edit forms on every backend: an untouched save keeps what was
//! stored (#1273); inline rows take no auto or generated input (#1271).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{Auto, FetcherPool as _, Pool};
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

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    fresh_table::<Doc>(pool).await;
    drop_table(pool, "aed_line").await;
    drop_table(pool, "aed_sline").await;
    fresh_table::<Order>(pool).await;
    fresh_table::<Line>(pool).await;
    fresh_table::<SOrder>(pool).await;
    fresh_table::<SLine>(pool).await;
}

/// Signal receivers are process-global; the SQLite arm takes no live lock.
fn signal_lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Inline child writes send one admin signal per row, pre and post (#1324).
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
    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    let mut want = vec![
        format!("pre_save {} change", ids[0]),
        format!("pre_delete {}", ids[1]),
        "pre_save  add".to_owned(),
        format!("post_save {} change", ids[0]),
        format!("post_delete {}", ids[1]),
        format!("post_save {added} add"),
    ];
    want.sort();
    assert_eq!(seen, want);
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
/// row added or edited without them saves (#1271).
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
    let at = html.find(&open).unwrap_or_else(|| panic!("no `{name}` control"));
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

/// A nullable `{}` and a value outside `choices` survive a save that
/// only renames the row (#1273).
async fn untouched_json_and_unknown_choice_are_kept(pool: &Pool) {
    let mut d = Doc {
        id: Auto::default(),
        name: "a".into(),
        meta: Some(serde_json::json!({})),
        status: "legacy".into(),
    };
    d.insert_pool(pool).await.expect("insert");
    let id = *d.id.get().expect("pk");
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
    assert_eq!(row.meta, Some(serde_json::json!({})), "nullable {{}} lost");
    assert_eq!(row.status, "legacy", "unknown choice overwritten");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        untouched_json_and_unknown_choice_are_kept,
        inline_rows_take_no_auto_or_generated_input,
        inline_child_writes_send_admin_signals,
    ],
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
    /// survive an untouched save (#1273).
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
        let (status, body) =
            send(&pool, "POST", &format!("/aed_pgkinds/{id}"), &encode(&pairs)).await;
        assert!(status.is_redirection(), "{status}: {body}");
        let got = PgKinds::objects().fetch(&pool).await.expect("fetch").remove(0);
        assert_eq!(got.name, "renamed");
        assert_eq!(got.tags.0, ["x", "y"]);
        assert_eq!(got.seats, Range::closed_open(1, 10));
        assert_eq!(got.attrs.0, attrs);
        assert_eq!(got.emb, Vector::new(vec![1.0, 2.0, 3.0]));
        assert_eq!(got.opt_emb, Some(Vector::new(vec![0.5, 0.0, 0.0])));
    }
}
