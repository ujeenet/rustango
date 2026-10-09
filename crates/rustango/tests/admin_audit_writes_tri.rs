//! Admin writes on `audit(...)` models audit in the write's transaction:
//! inline child rows (#2389); delete, soft delete, restore and bulk actions (#2390).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::audit::AuditLog;
use rustango::core::Model as _;
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "aaw_order")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "aaw_line", audit(track = "qty, note"))]
#[allow(dead_code)]
pub struct Line {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "aaw_order", on = "id")]
    pub order_id: i64,
    pub qty: i32,
    #[rustango(max_length = 32)]
    pub note: String,
}

/// A key longer than the audit log's `entity_pk` (255), so its audit row
/// fails on PG and MySQL.
#[derive(Model, Debug, Clone)]
#[rustango(table = "aaw_tag", audit(track = "note"))]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key, max_length = 300)]
    pub code: String,
    #[rustango(fk = "aaw_order", on = "id")]
    pub order_id: i64,
    #[rustango(max_length = 32)]
    pub note: String,
}

/// Hard delete. The long key fails the audit row on PG and MySQL.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "aaw_note",
    audit(track = "body"),
    admin(actions = "delete_selected")
)]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key, max_length = 300)]
    pub code: String,
    #[rustango(max_length = 32)]
    pub body: String,
}

/// Soft delete and restore.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "aaw_memo",
    audit(track = "body, deleted_at"),
    admin(actions = "delete_selected, restore_selected")
)]
#[allow(dead_code)]
pub struct Memo {
    #[rustango(primary_key, max_length = 300)]
    pub code: String,
    #[rustango(max_length = 32)]
    pub body: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

rustango::register_admin_inline!(
    parent = "aaw_order",
    child = "aaw_line",
    fk = "order_id",
    fields = &["qty", "note"],
    extra = 1,
);

rustango::register_admin_inline!(
    parent = "aaw_order",
    child = "aaw_tag",
    fk = "order_id",
    fields = &["code", "note"],
    extra = 1,
);

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "aaw_line").await;
    drop_table(pool, "aaw_tag").await;
    fresh_table::<Order>(pool).await;
    fresh_table::<Line>(pool).await;
    fresh_table::<Tag>(pool).await;
    fresh_table::<Note>(pool).await;
    fresh_table::<Memo>(pool).await;
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
    for t in ["aaw_order", "aaw_line", "aaw_tag", "aaw_note", "aaw_memo"] {
        AuditLog::delete_where("entity_table", t, pool)
            .await
            .expect("clear audit rows");
    }
}

/// Make the next audit write fail: a too-long key on PG and MySQL, which
/// check `entity_pk`'s length; no audit table on the private SQLite pool.
async fn break_audit(pool: &Pool) -> String {
    if pool.backend_name() == "sqlite" {
        rustango::testkit::matrix::drop_table(pool, "rustango_audit_log").await;
    }
    "k".repeat(300)
}

async fn post(pool: &Pool, uri: &str, form: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
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

async fn seed_order(pool: &Pool) -> i64 {
    let mut o = Order {
        id: Auto::default(),
        name: "o".into(),
    };
    o.insert_pool(pool).await.expect("insert order");
    *o.id.get().expect("pk")
}

async fn seed_line(pool: &Pool, order_id: i64, qty: i32, note: &str) -> i64 {
    let mut l = Line {
        id: Auto::default(),
        order_id,
        qty,
        note: note.into(),
    };
    l.insert_pool(pool).await.expect("insert line");
    *l.id.get().expect("pk")
}

/// `(operation, entity_pk, changes)` of every audit row for `table`.
async fn audit_rows(pool: &Pool, table: &str) -> Vec<(String, String, serde_json::Value)> {
    let mut rows: Vec<_> = AuditLog::objects()
        .filter("entity_table", table)
        .fetch(pool)
        .await
        .expect("fetch audit rows")
        .into_iter()
        .map(|r| (r.operation, r.entity_pk, r.changes))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// An inline update, delete and insert each write their audit row (#2389).
async fn inline_child_writes_are_audited(pool: &Pool) {
    let o = seed_order(pool).await;
    let l1 = seed_line(pool, o, 1, "a").await;
    let l2 = seed_line(pool, o, 2, "b").await;
    AuditLog::delete_where("entity_table", "aaw_line", pool)
        .await
        .expect("clear seed audit rows");
    let form = format!(
        "name=o&aaw_line-TOTAL_FORMS=3&aaw_line-INITIAL_FORMS=2\
         &aaw_line-0-id={l1}&aaw_line-0-qty=5&aaw_line-0-note=a\
         &aaw_line-1-id={l2}&aaw_line-1-qty=2&aaw_line-1-note=b&aaw_line-1-DELETE=on\
         &aaw_line-2-qty=3&aaw_line-2-note=c"
    );
    let (status, body) = post(pool, &format!("/aaw_order/{o}"), &form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let lines = Line::objects().fetch(pool).await.expect("fetch lines");
    assert_eq!(lines.len(), 2);
    let new = lines
        .iter()
        .find(|l| l.note == "c")
        .map(|l| l.id.get().expect("pk").to_string())
        .expect("inserted line");

    let rows = audit_rows(pool, "aaw_line").await;
    let ops: Vec<(&str, &str)> = rows
        .iter()
        .map(|(op, pk, _)| (op.as_str(), pk.as_str()))
        .collect();
    let (l1, l2) = (l1.to_string(), l2.to_string());
    assert_eq!(
        ops,
        [
            ("create", new.as_str()),
            ("delete", l2.as_str()),
            ("update", l1.as_str())
        ]
    );
    assert_eq!(rows[0].2["note"], "c", "{:?}", rows[0].2);
    assert_eq!(rows[1].2["qty"], 2, "{:?}", rows[1].2);
    assert_eq!(rows[2].2["qty"]["before"], 1, "{:?}", rows[2].2);
    assert_eq!(rows[2].2["qty"]["after"], 5, "{:?}", rows[2].2);
}

/// An inline row whose audit row cannot be written saves nothing (#2389).
async fn inline_child_audit_failure_saves_nothing(pool: &Pool) {
    let o = seed_order(pool).await;
    let code = break_audit(pool).await;
    let form = format!(
        "name=renamed&aaw_tag-TOTAL_FORMS=1&aaw_tag-INITIAL_FORMS=0\
         &aaw_tag-0-code={code}&aaw_tag-0-note=n"
    );
    let (status, body) = post(pool, &format!("/aaw_order/{o}"), &form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Nothing was saved"), "{body}");
    assert_eq!(Tag::objects().count(pool).await.expect("count"), 0);
    let orders = Order::objects().fetch(pool).await.expect("fetch orders");
    assert_eq!(orders[0].name, "o", "the parent edit committed");
}

/// One note and one live memo under `code`, written without an audit row.
async fn seed_note_and_memo(pool: &Pool, code: &str) {
    use rustango::core::InsertQuery;
    for (table, schema) in [("note", Note::SCHEMA), ("memo", Memo::SCHEMA)] {
        let q = InsertQuery::new(
            schema,
            vec!["code", "body"],
            vec![code.into(), table.into()],
        );
        rustango::sql::insert_pool(pool, &q).await.expect("seed");
    }
}

async fn note_count(pool: &Pool) -> i64 {
    Note::objects().count(pool).await.expect("count notes")
}

async fn memo_deleted(pool: &Pool) -> bool {
    let memos = Memo::objects().fetch(pool).await.expect("fetch memos");
    memos[0].deleted_at.is_some()
}

/// Single and bulk deletes, soft deletes and restores audit as before.
async fn deletes_and_bulk_actions_are_audited(pool: &Pool) {
    let ops = |table: &'static str| async move {
        let rows = audit_rows(pool, table).await;
        rows.into_iter().map(|r| r.0).collect::<Vec<_>>()
    };
    seed_note_and_memo(pool, "a").await;
    let (status, body) = post(pool, "/aaw_note/a/delete", "").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(note_count(pool).await, 0);
    let (status, body) = post(pool, "/aaw_memo/a/delete", "").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(memo_deleted(pool).await);
    let form = "action=restore_selected&_selected=a&trashed=1";
    let (status, body) = post(pool, "/aaw_memo/__action", form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(!memo_deleted(pool).await);
    // A bulk action audits as an `update` tagged with its name.
    assert_eq!(ops("aaw_memo").await, ["soft_delete", "update"]);

    seed_note_and_memo(pool, "b").await;
    let form = "action=delete_selected&_selected=b";
    let (status, body) = post(pool, "/aaw_note/__action", form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(note_count(pool).await, 0);
    assert_eq!(ops("aaw_note").await, ["delete", "delete"]);
}

/// A delete, soft delete or restore whose audit row cannot be written
/// writes nothing (#2390). Each write gets its own rows; leaks are
/// collected so one run shows every path that commits.
async fn delete_audit_failure_keeps_the_row(pool: &Pool) {
    use rustango::core::{Assignment, Filter, Op, SqlValue, UpdateQuery, WhereExpr};
    let base = break_audit(pool).await;
    let key = |i: usize| format!("{i}{}", &base[1..]);
    let note_gone = |code: String| async move {
        Note::objects()
            .filter("code", code)
            .count(pool)
            .await
            .expect("count")
            == 0
    };
    let memo_stamped = |code: String| async move {
        let memos = Memo::objects().filter("code", code).fetch(pool).await;
        memos.expect("fetch")[0].deleted_at.is_some()
    };
    for i in 0..5 {
        seed_note_and_memo(pool, &key(i)).await;
    }
    // Trash memo 4 without an audit row, to restore it below.
    let stamp = UpdateQuery::new(
        Memo::SCHEMA,
        vec![Assignment::new(
            "deleted_at",
            SqlValue::DateTime(chrono::Utc::now()),
        )],
        WhereExpr::Predicate(Filter::new("code", Op::Eq, SqlValue::String(key(4)))),
    );
    rustango::sql::update_pool(pool, &stamp)
        .await
        .expect("trash");

    let mut leaks = Vec::new();
    let mut check = |what: &'static str, (status, _): (StatusCode, String), wrote: bool| {
        if status.is_redirection() || wrote {
            leaks.push(format!("{what}: {status}, wrote={wrote}"));
        }
    };
    let r = post(pool, &format!("/aaw_note/{}/delete", key(0)), "").await;
    check("delete", r, note_gone(key(0)).await);
    let r = post(pool, &format!("/aaw_memo/{}/delete", key(1)), "").await;
    check("soft delete", r, memo_stamped(key(1)).await);
    let form = |action: &str, i: usize| format!("action={action}&_selected={}&trashed=1", key(i));
    let r = post(pool, "/aaw_note/__action", &form("delete_selected", 2)).await;
    check("bulk delete", r, note_gone(key(2)).await);
    let r = post(pool, "/aaw_memo/__action", &form("delete_selected", 3)).await;
    check("bulk soft delete", r, memo_stamped(key(3)).await);
    let r = post(pool, "/aaw_memo/__action", &form("restore_selected", 4)).await;
    check("bulk restore", r, !memo_stamped(key(4)).await);
    assert!(
        leaks.is_empty(),
        "committed without an audit row: {leaks:#?}"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        inline_child_writes_are_audited,
        inline_child_audit_failure_saves_nothing,
        deletes_and_bulk_actions_are_audited,
        delete_audit_failure_keeps_the_row,
    ],
}
