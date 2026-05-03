//! Live integration test for the v0.15 sub-slice F.1 ContentType
//! framework — bootstrap the table, seed it from inventory, look up
//! by `for_model::<T>` / `by_natural_key` / `by_id` / `all`.
//!
//! Activated when `DATABASE_URL` is set (the same env var the rest of
//! the live tests use); skips silently otherwise.
//!
//! Schema is owned: every test starts by `DROP TABLE IF EXISTS
//! rustango_content_types CASCADE` then re-applies the ContentType
//! DDL via `apply_all` so re-runs are idempotent. Tests share the
//! one table so they're serialized via a tokio mutex.

use std::sync::OnceLock;

use rustango::contenttypes::{self, ContentType};
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_live_post")]
#[rustango(app = "blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_live_user")]
#[rustango(app = "auth")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub username: String,
}

fn ct_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL failed");
    // Reset the tables involved in these tests so re-runs are clean.
    for tbl in ["rustango_content_types", "ct_live_post", "ct_live_user"] {
        let drop_sql = format!(r#"DROP TABLE IF EXISTS "{tbl}" CASCADE"#);
        let _ = sqlx::query(&drop_sql).execute(&pool).await;
    }
    rustango::migrate::apply_all(&pool)
        .await
        .expect("apply_all");
    Some(pool)
}

#[tokio::test]
async fn ensure_seeded_inserts_a_row_per_model() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    let inserted = contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded");
    assert!(
        inserted >= 2,
        "expected at least the two test models seeded, got {inserted}"
    );
    // Re-running should be a no-op.
    let inserted_again = contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded idempotent");
    assert_eq!(
        inserted_again, 0,
        "re-running ensure_seeded should insert nothing"
    );
}

#[tokio::test]
async fn for_model_resolves_to_correct_row() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let ct = ContentType::for_model::<Post>(&pool)
        .await
        .expect("for_model")
        .expect("Post ContentType row exists");
    assert_eq!(ct.app_label, "blog");
    assert_eq!(ct.model_name, "post");
    assert_eq!(ct.table, "ct_live_post");
    assert!(ct.id.get().is_some(), "id should be populated by RETURNING");
}

#[tokio::test]
async fn by_natural_key_round_trips() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let ct = ContentType::by_natural_key(&pool, "auth", "user")
        .await
        .expect("by_natural_key")
        .expect("auth.user exists");
    assert_eq!(ct.table, "ct_live_user");
    let pk = ct.id.get().copied().expect("auto pk populated");

    // by_id should return the same row.
    let by_id = ContentType::by_id(&pool, pk)
        .await
        .expect("by_id")
        .expect("pk exists");
    assert_eq!(by_id.app_label, "auth");
    assert_eq!(by_id.model_name, "user");
}

#[tokio::test]
async fn all_returns_seeded_rows_ordered() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let rows = ContentType::all(&pool).await.expect("all");
    assert!(rows.len() >= 2, "at least two seeded models");
    // Confirm sort order: app_label asc, model_name asc.
    let mut last: Option<(String, String)> = None;
    for ct in &rows {
        if let Some(prev) = &last {
            assert!(
                (&prev.0, &prev.1) <= (&ct.app_label, &ct.model_name),
                "rows out of order at {} / {}",
                ct.app_label,
                ct.model_name,
            );
        }
        last = Some((ct.app_label.clone(), ct.model_name.clone()));
    }
}

#[tokio::test]
async fn ensure_seeded_skips_content_type_table_itself() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let row = ContentType::by_natural_key(&pool, "project", "contenttype")
        .await
        .expect("query");
    assert!(
        row.is_none(),
        "ContentType should not have a self-referential row"
    );
    let alt = ContentType::by_natural_key(&pool, "contenttypes", "contenttype")
        .await
        .expect("query");
    assert!(alt.is_none(), "ContentType should not seed itself");
}
