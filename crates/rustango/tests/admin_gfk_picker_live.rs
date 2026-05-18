//! Live test for the GenericForeignKey `<select>` picker on the
//! standalone create/edit form. Issue #244.
//!
//! Without this slice, a model carrying `#[rustango(generic_fk(...))]`
//! shows raw `<input type="number">` widgets for the `content_type_id`
//! column on the standalone `/__admin/<table>/new` form. The operator
//! has to memorize the integer CT id of the target. This test asserts
//! the column now renders as a `<select>` populated from the seeded
//! ContentType table.

#![cfg(feature = "postgres")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::contenttypes;
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfp_post")]
#[rustango(app = "gfp_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfp_article")]
#[rustango(app = "gfp_blog")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gfp_attachment")]
#[rustango(app = "gfp_blog")]
#[rustango(generic_fk(name = "owner", ct_column = "content_type_id", pk_column = "object_pk"))]
#[allow(dead_code)]
pub struct Attachment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
    pub object_pk: i64,
    #[rustango(max_length = 200)]
    pub file_path: String,
}

async fn fresh(pool: &sqlx::PgPool) {
    let p = rustango::sql::Pool::from(pool.clone());
    contenttypes::ensure_seeded(&p).await.unwrap();
    for t in ["gfp_attachment", "gfp_article", "gfp_post"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "gfp_post" (id BIGSERIAL PRIMARY KEY, title VARCHAR(200) NOT NULL)"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gfp_article" (id BIGSERIAL PRIMARY KEY, title VARCHAR(200) NOT NULL)"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gfp_attachment" (
               id BIGSERIAL PRIMARY KEY,
               content_type_id BIGINT NOT NULL,
               object_pk BIGINT NOT NULL,
               file_path VARCHAR(200) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn create_form_renders_ct_column_as_select_picker() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri("/gfp_attachment/new")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // The ct_column now renders as a `<select>` rather than a raw
    // number input. Look for the `name="content_type_id"` select shell.
    assert!(
        html.contains(r#"<select name="content_type_id""#),
        "ct_column should render as a <select>: {html}"
    );
    // Sentinel placeholder option.
    assert!(
        html.contains("— choose target —"),
        "picker placeholder option missing: {html}"
    );
    // Every seeded ContentType becomes an option labeled
    // "<app_label>.<model_name>" — Post + Article are in the
    // registry from this test module, so both must appear.
    assert!(
        html.contains("gfp_blog.post"),
        "Post should appear as a picker option: {html}"
    );
    assert!(
        html.contains("gfp_blog.article"),
        "Article should appear as a picker option: {html}"
    );
    // The raw `<input type="number" name="content_type_id"` MUST be
    // gone — the picker fully replaces it.
    assert!(
        !html.contains(r#"<input type="number" step="1" name="content_type_id""#),
        "raw integer input must NOT render alongside the picker: {html}"
    );
    // pk_column stays as a plain integer input — v1 picker scope is
    // just ct_column; the pk_column typeahead is a deferred follow-up.
    assert!(
        html.contains(r#"name="object_pk""#),
        "pk_column input still renders as a regular field: {html}"
    );
}

#[tokio::test]
async fn edit_form_renders_picker_with_current_ct_preselected() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let p = rustango::sql::Pool::from(pool.clone());

    // Seed a Post + an Attachment pointing at it.
    let mut post = Post {
        id: Auto::Unset,
        title: "Hello".into(),
    };
    post.save_pool(&p).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    let mut attach = Attachment {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        file_path: "/files/x.pdf".into(),
    };
    attach.set_owner_for::<Post>(&p, post_pk).await.unwrap();
    attach.save_pool(&p).await.unwrap();
    let attach_pk = *attach.id.get().unwrap();

    // The Post's CT id — what the picker should mark as selected.
    let post_ct_id = *contenttypes::ContentType::get_for_model::<Post>(&p)
        .await
        .unwrap()
        .unwrap()
        .id
        .get()
        .unwrap();

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/gfp_attachment/{attach_pk}/edit"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // The Post's CT option must carry `selected`.
    let expected = format!(r#"<option value="{post_ct_id}" selected>"#);
    assert!(
        html.contains(&expected),
        "expected option `{expected}` to be selected, in: {html}"
    );
}

#[tokio::test]
async fn create_form_for_model_without_generic_fk_keeps_default_inputs() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // Post has no generic_fk — its create form should render plain
    // inputs, no `<select>` (the picker MUST NOT leak across models).
    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri("/gfp_post/new")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        !html.contains("— choose target —"),
        "non-generic-fk model should not render the picker placeholder: {html}"
    );
}
