//! Live test for the editable generic-inline flow (#243).
//!
//! Mirrors `admin_inlines_edit_live.rs` (PR #238) but uses
//! `register_admin_inline_generic!` so the WHERE pins both
//! `(content_type_id, object_pk)` instead of one FK column.
//!
//! Exercises one full round-trip: GET edit page → POST with mixed
//! UPDATE + DELETE + INSERT + blank-extra → verify via direct SQL
//! that the writes landed.

#![cfg(feature = "postgres")]

use std::collections::HashMap;

use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use rustango::admin::inlines::InlineKind;
use rustango::contenttypes;
use rustango::register_admin_inline_generic;
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
#[rustango(table = "gige_post")]
#[rustango(app = "gige_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gige_tag")]
#[rustango(app = "gige_blog")]
#[rustango(generic_fk(
    name = "content_object",
    ct_column = "content_type_id",
    pk_column = "object_pk"
))]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
    pub object_pk: i64,
    #[rustango(max_length = 40)]
    pub name: String,
}

register_admin_inline_generic!(
    parent = "gige_post",
    child = "gige_tag",
    ct = "content_type_id",
    pk = "object_pk",
    kind = InlineKind::Tabular,
    label = "Tags",
    fields = &["name"],
    extra = 2,
);

async fn fresh(pool: &sqlx::PgPool) {
    let p = rustango::sql::Pool::from(pool.clone());
    contenttypes::ensure_seeded(&p).await.unwrap();
    for t in ["gige_tag", "gige_post"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "gige_post" (
               id BIGSERIAL PRIMARY KEY,
               title VARCHAR(200) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gige_tag" (
               id BIGSERIAL PRIMARY KEY,
               content_type_id BIGINT NOT NULL,
               object_pk BIGINT NOT NULL,
               name VARCHAR(40) NOT NULL
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
async fn edit_page_round_trips_generic_inline_update_delete_insert() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let p = rustango::sql::Pool::from(pool.clone());

    // Seed one Post + two attached tags.
    let mut post = Post {
        id: Auto::Unset,
        title: "Tagged".into(),
    };
    post.save_pool(&p).await.unwrap();
    let post_pk = *post.id.get().unwrap();

    let mut tag_keep = Tag {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        name: "keep".into(),
    };
    tag_keep
        .set_content_object_for::<Post>(&p, post_pk)
        .await
        .unwrap();
    tag_keep.save_pool(&p).await.unwrap();
    let keep_id = *tag_keep.id.get().unwrap();

    let mut tag_drop = Tag {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        name: "drop".into(),
    };
    tag_drop
        .set_content_object_for::<Post>(&p, post_pk)
        .await
        .unwrap();
    tag_drop.save_pool(&p).await.unwrap();
    let drop_id = *tag_drop.id.get().unwrap();

    // GET edit form — assert the generic-inline FormSet renders.
    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/gige_post/{post_pk}/edit"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Management form fields rendered for the generic inline.
    assert!(
        html.contains(r#"name="gige_tag-TOTAL_FORMS""#),
        "TOTAL_FORMS missing: {html}"
    );
    // 2 existing rows + 2 extras = 4 total.
    assert!(
        html.contains(r#"value="4""#),
        "total_forms=4 not present: {html}"
    );
    // Prefix-mangled `name` input for row 0.
    assert!(
        html.contains(r#"name="gige_tag-0-name""#),
        "row 0 name input missing: {html}"
    );
    // DELETE checkbox on existing rows.
    assert!(
        html.contains(r#"name="gige_tag-0-DELETE""#),
        "row 0 DELETE checkbox missing: {html}"
    );
    // Hidden PK input.
    assert!(
        html.contains(r#"name="gige_tag-0-id""#),
        "hidden PK input missing on row 0: {html}"
    );

    // POST edit:
    //   row 0 (keep): rename
    //   row 1 (drop): DELETE
    //   row 2 (extra): INSERT a fresh tag
    //   row 3 (extra): blank — no-op
    let mut form: HashMap<&str, &str> = HashMap::new();
    form.insert("title", "Tagged");
    form.insert("gige_tag-TOTAL_FORMS", "4");
    form.insert("gige_tag-INITIAL_FORMS", "2");
    form.insert("gige_tag-MAX_NUM_FORMS", "");

    let keep_id_s = keep_id.to_string();
    let drop_id_s = drop_id.to_string();
    form.insert("gige_tag-0-id", keep_id_s.as_str());
    form.insert("gige_tag-0-name", "keep-renamed");

    form.insert("gige_tag-1-id", drop_id_s.as_str());
    form.insert("gige_tag-1-name", "drop");
    form.insert("gige_tag-1-DELETE", "on");

    form.insert("gige_tag-2-name", "brand-new");
    // gige_tag-3-* intentionally absent (blank extra).

    let payload = urlencode(&form);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/gige_post/{post_pk}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(payload))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert!(
        res.status() == StatusCode::SEE_OTHER || res.status() == StatusCode::FOUND,
        "POST should redirect; got {}",
        res.status()
    );

    // Verify via direct SQL.
    let rows: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        r#"SELECT id, name, content_type_id, object_pk FROM "gige_tag"
           ORDER BY id"#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    let post_ct = contenttypes::ContentType::get_for_model::<Post>(&p)
        .await
        .unwrap()
        .unwrap();
    let post_ct_id = *post_ct.id.get().unwrap();

    assert_eq!(
        rows.len(),
        2,
        "expected 2 tags after update+delete+insert, got {rows:?}"
    );
    // Kept tag got renamed.
    let kept = rows.iter().find(|r| r.0 == keep_id).expect("keep survived");
    assert_eq!(kept.1, "keep-renamed", "UPDATE didn't apply: {kept:?}");
    // Polymorphic columns unchanged (UPDATE skips both).
    assert_eq!(kept.2, post_ct_id, "ct_column drift after UPDATE: {kept:?}");
    assert_eq!(kept.3, post_pk, "pk_column drift after UPDATE: {kept:?}");
    // Dropped row gone.
    assert!(
        !rows.iter().any(|r| r.0 == drop_id),
        "DELETE didn't remove drop_id: {rows:?}"
    );
    // Inserted row landed with BOTH polymorphic columns pinned.
    let inserted = rows
        .iter()
        .find(|r| r.1 == "brand-new")
        .expect("new tag not present");
    assert_eq!(
        inserted.2, post_ct_id,
        "INSERT didn't pin ct_column: {inserted:?}"
    );
    assert_eq!(
        inserted.3, post_pk,
        "INSERT didn't pin pk_column: {inserted:?}"
    );
}

#[tokio::test]
async fn generic_inline_update_cannot_reparent_via_form_payload() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;
    let p = rustango::sql::Pool::from(pool.clone());

    let mut post_a = Post {
        id: Auto::Unset,
        title: "A".into(),
    };
    post_a.save_pool(&p).await.unwrap();
    let post_a_pk = *post_a.id.get().unwrap();

    let mut post_b = Post {
        id: Auto::Unset,
        title: "B".into(),
    };
    post_b.save_pool(&p).await.unwrap();
    let post_b_pk = *post_b.id.get().unwrap();

    let mut tag = Tag {
        id: Auto::Unset,
        content_type_id: 0,
        object_pk: 0,
        name: "anchor".into(),
    };
    tag.set_content_object_for::<Post>(&p, post_a_pk)
        .await
        .unwrap();
    tag.save_pool(&p).await.unwrap();
    let tag_id = *tag.id.get().unwrap();

    // POST an edit of Post A that tries to reparent the tag to Post B
    // by submitting `gige_tag-0-object_pk = <post_b_pk>` inline.
    let post_b_pk_s = post_b_pk.to_string();
    let tag_id_s = tag_id.to_string();
    let mut form: HashMap<&str, &str> = HashMap::new();
    form.insert("title", "A");
    form.insert("gige_tag-TOTAL_FORMS", "1");
    form.insert("gige_tag-INITIAL_FORMS", "1");
    form.insert("gige_tag-MAX_NUM_FORMS", "");
    form.insert("gige_tag-0-id", tag_id_s.as_str());
    form.insert("gige_tag-0-name", "anchor-renamed");
    // Malicious — try to flip object_pk to Post B's pk.
    form.insert("gige_tag-0-object_pk", post_b_pk_s.as_str());

    let app = rustango::admin::router(pool.clone());
    let payload = urlencode(&form);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/gige_post/{post_a_pk}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(payload))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert!(
        res.status() == StatusCode::SEE_OTHER || res.status() == StatusCode::FOUND,
        "POST should redirect; got {}",
        res.status()
    );

    // The tag's name got updated but its polymorphic key did NOT — the
    // POST handler skips ct_column + pk_column on UPDATE to prevent
    // reparenting.
    let (name, object_pk): (String, i64) =
        sqlx::query_as(r#"SELECT name, object_pk FROM "gige_tag" WHERE id = $1"#)
            .bind(tag_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "anchor-renamed", "name update should still apply");
    assert_eq!(
        object_pk, post_a_pk,
        "object_pk must NOT change — slice 2 skips polymorphic columns on UPDATE"
    );
}
