//! Live test for the editable inline flow (#50 slice 2).
//!
//! Spins up two models (`ile_blog` + `ile_blog_post`), registers an
//! inline with `extra = 2` blank rows, then exercises the round-trip:
//!
//! 1. GET the edit page — assert the management form + per-row inputs
//!    render with prefix-mangled names.
//! 2. POST a payload that UPDATEs one existing row, DELETEs another,
//!    and INSERTs a fresh row via one of the blank `extra` slots.
//! 3. Re-GET and assert all three writes took effect.

#![cfg(feature = "postgres")]

use std::collections::HashMap;

use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use rustango::admin::inlines::InlineKind;
use rustango::register_admin_inline;
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::Model;
use tower::ServiceExt;

use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

#[derive(Model, Debug)]
#[rustango(table = "ile_blog")]
#[allow(dead_code)]
pub struct Blog {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug)]
#[rustango(table = "ile_blog_post")]
#[allow(dead_code)]
pub struct BlogPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "ile_blog", on = "id")]
    pub blog_id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 500)]
    pub body: String,
}

register_admin_inline!(
    parent = "ile_blog",
    child = "ile_blog_post",
    fk = "blog_id",
    kind = InlineKind::Tabular,
    label = "Posts",
    fields = &["title", "body"],
    extra = 2,
);

async fn fresh(pool: &sqlx::PgPool) {
    for t in ["ile_blog_post", "ile_blog"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "ile_blog" (
               id BIGSERIAL PRIMARY KEY,
               name VARCHAR(100) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "ile_blog_post" (
               id BIGSERIAL PRIMARY KEY,
               blog_id BIGINT NOT NULL REFERENCES "ile_blog"(id),
               title VARCHAR(200) NOT NULL,
               body VARCHAR(500) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

fn urlencode(form: &HashMap<&str, &str>) -> String {
    let mut out = String::new();
    for (i, (k, v)) in form.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&urlencoding::encode(k));
        out.push('=');
        out.push_str(&urlencoding::encode(v));
    }
    out
}

#[tokio::test]
async fn edit_page_round_trips_update_delete_insert_across_inline_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut blog = Blog {
        id: Auto::default(),
        name: "Edit Inlines Blog".into(),
    };
    blog.insert(&pool).await.unwrap();
    let blog_id = *blog.id.get().expect("PK assigned");

    let mut post_keep = BlogPost {
        id: Auto::default(),
        blog_id,
        title: "Keep me".into(),
        body: "original body".into(),
    };
    post_keep.insert(&pool).await.unwrap();
    let keep_id = *post_keep.id.get().expect("PK assigned");

    let mut post_drop = BlogPost {
        id: Auto::default(),
        blog_id,
        title: "Drop me".into(),
        body: "to be removed".into(),
    };
    post_drop.insert(&pool).await.unwrap();
    let drop_id = *post_drop.id.get().expect("PK assigned");

    // ----- 1. GET edit form -----
    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/ile_blog/{blog_id}/edit"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Management form fields rendered with the child table as prefix.
    assert!(
        html.contains(r#"name="ile_blog_post-TOTAL_FORMS""#),
        "TOTAL_FORMS missing: {html}"
    );
    // 2 existing rows + 2 extras = 4 total.
    assert!(
        html.contains(r#"value="4""#),
        "total_forms=4 not present: {html}"
    );
    // Prefix-mangled inputs for the first existing row.
    assert!(
        html.contains(r#"name="ile_blog_post-0-title""#),
        "row 0 title input missing: {html}"
    );
    // DELETE checkbox on existing rows only.
    assert!(
        html.contains(r#"name="ile_blog_post-0-DELETE""#),
        "row 0 DELETE checkbox missing: {html}"
    );
    // No DELETE checkbox on the blank extras (idx 2 + 3).
    assert!(
        !html.contains(r#"name="ile_blog_post-2-DELETE""#),
        "extras shouldn't render DELETE: {html}"
    );
    // Hidden PK input round-trips the existing row identity.
    assert!(
        html.contains(r#"name="ile_blog_post-0-id""#),
        "hidden PK input missing on row 0: {html}"
    );

    // ----- 2. POST edit -----
    //
    // Row 0 (keep): change title.
    // Row 1 (drop): mark DELETE.
    // Row 2 (extra): INSERT a fresh post.
    // Row 3 (extra): leave blank — should be a no-op.
    let mut form: HashMap<&str, &str> = HashMap::new();
    form.insert("name", "Edit Inlines Blog");
    form.insert("ile_blog_post-TOTAL_FORMS", "4");
    form.insert("ile_blog_post-INITIAL_FORMS", "2");
    form.insert("ile_blog_post-MAX_NUM_FORMS", "");

    let keep_id_s = keep_id.to_string();
    let drop_id_s = drop_id.to_string();
    form.insert("ile_blog_post-0-id", keep_id_s.as_str());
    form.insert("ile_blog_post-0-title", "Renamed keeper");
    form.insert("ile_blog_post-0-body", "updated body");

    form.insert("ile_blog_post-1-id", drop_id_s.as_str());
    form.insert("ile_blog_post-1-title", "Drop me");
    form.insert("ile_blog_post-1-body", "to be removed");
    form.insert("ile_blog_post-1-DELETE", "on");

    form.insert("ile_blog_post-2-title", "Brand new");
    form.insert("ile_blog_post-2-body", "freshly inserted");

    // Row 3: intentionally blank — empty extras stay empty.

    let payload = urlencode(&form);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/ile_blog/{blog_id}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(payload))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert!(
        res.status() == StatusCode::SEE_OTHER
            || res.status() == StatusCode::FOUND
            || res.status() == StatusCode::TEMPORARY_REDIRECT,
        "POST should redirect; got {}",
        res.status()
    );

    // ----- 3. Verify the writes via direct SQL -----
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        r#"SELECT id, title, body FROM "ile_blog_post" WHERE blog_id = $1 ORDER BY id"#,
    )
    .bind(blog_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    assert_eq!(
        rows.len(),
        2,
        "expected 2 rows after update+delete+insert, got {:?}",
        rows
    );
    // Kept row got renamed.
    let kept = rows
        .iter()
        .find(|r| r.0 == keep_id)
        .expect("keeper survived");
    assert_eq!(kept.1, "Renamed keeper", "UPDATE didn't apply: {kept:?}");
    assert_eq!(kept.2, "updated body");
    // Dropped row is gone.
    assert!(
        !rows.iter().any(|r| r.0 == drop_id),
        "DELETE didn't remove drop_id: {rows:?}"
    );
    // Inserted row landed with the parent FK.
    assert!(
        titles.contains(&"Brand new"),
        "INSERT didn't add Brand new: titles={titles:?}"
    );
}

#[tokio::test]
async fn edit_page_skips_blank_extra_rows_without_failures() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut blog = Blog {
        id: Auto::default(),
        name: "Blank Extras Blog".into(),
    };
    blog.insert(&pool).await.unwrap();
    let blog_id = *blog.id.get().expect("PK assigned");

    let app = rustango::admin::router(pool.clone());
    let mut form: HashMap<&str, &str> = HashMap::new();
    form.insert("name", "Blank Extras Blog");
    form.insert("ile_blog_post-TOTAL_FORMS", "2");
    form.insert("ile_blog_post-INITIAL_FORMS", "0");
    form.insert("ile_blog_post-MAX_NUM_FORMS", "");
    // Both extras blank.
    form.insert("ile_blog_post-0-title", "");
    form.insert("ile_blog_post-0-body", "");
    form.insert("ile_blog_post-1-title", "");
    form.insert("ile_blog_post-1-body", "");

    let payload = urlencode(&form);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/ile_blog/{blog_id}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(payload))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert!(
        res.status() == StatusCode::SEE_OTHER || res.status() == StatusCode::FOUND,
        "should redirect even with all-empty extras, got {}",
        res.status()
    );

    // Verify no rows were created.
    let count: (i64,) =
        sqlx::query_as(r#"SELECT COUNT(*) FROM "ile_blog_post" WHERE blog_id = $1"#)
            .bind(blog_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 0, "blank extras shouldn't have created rows");
}
