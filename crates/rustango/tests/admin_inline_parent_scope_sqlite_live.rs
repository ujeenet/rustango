//! Inline FormSet writes stay under the edited parent and pass the
//! child table's own admin gates (#1667).

#![cfg(all(feature = "sqlite", feature = "admin"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::admin::inlines::InlineKind;
use rustango::core::{Model as _, SqlValue};
use rustango::register_admin_inline;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug)]
#[rustango(table = "ips_parent")]
#[allow(dead_code)]
pub struct IpsParent {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug)]
#[rustango(table = "ips_child")]
#[allow(dead_code)]
pub struct IpsChild {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(fk = "ips_parent", on = "id")]
    pub parent_id: i64,
    #[rustango(max_length = 100)]
    pub title: String,
}

register_admin_inline!(
    parent = "ips_parent",
    child = "ips_child",
    fk = "parent_id",
    kind = InlineKind::Tabular,
    fields = &["parent_id", "title"],
    extra = 1,
);

/// Parents 1 and 2, child 1 under parent 1, child 2 under parent 2.
async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for sql in [
        r#"CREATE TABLE "ips_parent" ("id" INTEGER PRIMARY KEY AUTOINCREMENT, "name" TEXT NOT NULL)"#,
        r#"CREATE TABLE "ips_child" ("id" INTEGER PRIMARY KEY AUTOINCREMENT,
            "parent_id" INTEGER NOT NULL REFERENCES "ips_parent"("id"), "title" TEXT NOT NULL)"#,
        r#"INSERT INTO "ips_parent" ("id", "name") VALUES (1, 'p1'), (2, 'p2')"#,
        r#"INSERT INTO "ips_child" ("id", "parent_id", "title") VALUES (1, 1, 'c1'), (2, 2, 'c2')"#,
    ] {
        rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .expect("setup");
    }
    pool
}

/// `(id, parent_id, title)` of every child, by id.
async fn children(pool: &Pool) -> Vec<(i64, i64, String)> {
    let rows = rustango::sql::select_rows_as_json(
        pool,
        &rustango::core::SelectQuery::new(IpsChild::SCHEMA),
        &IpsChild::SCHEMA.scalar_fields().collect::<Vec<_>>(),
    )
    .await
    .expect("select children");
    let mut out: Vec<(i64, i64, String)> = rows
        .iter()
        .map(|r| {
            (
                r["id"].as_i64().unwrap(),
                r["parent_id"].as_i64().unwrap(),
                r["title"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

async fn parent_name(pool: &Pool, id: i64) -> String {
    let row = rustango::sql::select_one_row_as_json(
        pool,
        &rustango::core::SelectQuery::by_pk(IpsParent::SCHEMA, "id", SqlValue::I64(id)),
        &IpsParent::SCHEMA.scalar_fields().collect::<Vec<_>>(),
    )
    .await
    .expect("select parent")
    .expect("parent exists");
    row["name"].as_str().unwrap().to_owned()
}

async fn post(app: axum::Router, uri: &str, form: &[(&str, &str)]) -> StatusCode {
    let body = form
        .iter()
        .map(|(k, v)| format!("{k}={}", v.replace(' ', "+")))
        .collect::<Vec<_>>()
        .join("&");
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

fn app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

fn is_redirect(s: StatusCode) -> bool {
    s == StatusCode::SEE_OTHER || s == StatusCode::FOUND
}

const MGMT: [(&str, &str); 3] = [
    ("ips_child-TOTAL_FORMS", "1"),
    ("ips_child-INITIAL_FORMS", "1"),
    ("ips_child-MAX_NUM_FORMS", ""),
];

#[tokio::test]
async fn update_with_other_parents_child_pk_changes_nothing() {
    let pool = fresh_pool().await;
    let mut form = vec![
        ("name", "p1 renamed"),
        ("ips_child-0-id", "2"),
        ("ips_child-0-parent_id", "2"),
        ("ips_child-0-title", "hijacked"),
    ];
    form.extend(MGMT);
    let status = post(app(pool.clone()), "/ips_parent/1", &form).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        children(&pool).await,
        vec![(1, 1, "c1".into()), (2, 2, "c2".into())]
    );
    assert_eq!(
        parent_name(&pool, 1).await,
        "p1",
        "refused before any write"
    );
}

#[tokio::test]
async fn delete_with_other_parents_child_pk_deletes_nothing() {
    let pool = fresh_pool().await;
    let mut form = vec![
        ("name", "p1"),
        ("ips_child-0-id", "2"),
        ("ips_child-0-parent_id", "2"),
        ("ips_child-0-title", "c2"),
        ("ips_child-0-DELETE", "on"),
    ];
    form.extend(MGMT);
    let status = post(app(pool.clone()), "/ips_parent/1", &form).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(children(&pool).await.len(), 2, "child 2 survived");
}

#[tokio::test]
async fn own_rows_update_and_insert_pins_parent() {
    let pool = fresh_pool().await;
    let form = [
        ("name", "p1"),
        ("ips_child-TOTAL_FORMS", "2"),
        ("ips_child-INITIAL_FORMS", "1"),
        ("ips_child-MAX_NUM_FORMS", ""),
        ("ips_child-0-id", "1"),
        ("ips_child-0-parent_id", "2"),
        ("ips_child-0-title", "c1 renamed"),
        ("ips_child-1-parent_id", "2"),
        ("ips_child-1-title", "new"),
    ];
    let status = post(app(pool.clone()), "/ips_parent/1", &form).await;

    assert!(is_redirect(status), "got {status}");
    assert_eq!(
        children(&pool).await,
        vec![
            (1, 1, "c1 renamed".into()),
            (2, 2, "c2".into()),
            (3, 1, "new".into()),
        ]
    );
}

#[tokio::test]
async fn read_only_child_table_refuses_inline_writes() {
    let pool = fresh_pool().await;
    let ro_app = || {
        rustango::admin::Builder::new(pool.clone())
            .admin_prefix("")
            .read_only(["ips_child"])
            .build()
    };

    let mut update = vec![
        ("name", "p1"),
        ("ips_child-0-id", "1"),
        ("ips_child-0-title", "changed"),
    ];
    update.extend(MGMT);
    let mut delete = update.clone();
    delete.push(("ips_child-0-DELETE", "on"));
    let insert = [
        ("name", "p1"),
        ("ips_child-TOTAL_FORMS", "1"),
        ("ips_child-INITIAL_FORMS", "0"),
        ("ips_child-MAX_NUM_FORMS", ""),
        ("ips_child-0-title", "new"),
    ];
    for form in [&update[..], &delete[..], &insert[..]] {
        let status = post(ro_app(), "/ips_parent/1", form).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "form {form:?}");
    }
    assert_eq!(
        children(&pool).await,
        vec![(1, 1, "c1".into()), (2, 2, "c2".into())]
    );

    // The edit page offers no FormSet for the read-only child.
    let res = ro_app()
        .oneshot(
            Request::builder()
                .uri("/ips_parent/1/edit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1_000_000)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(!html.contains("ips_child-TOTAL_FORMS"), "{html}");
}
