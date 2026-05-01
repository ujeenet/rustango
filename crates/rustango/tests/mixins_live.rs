//! Live tests for v0.12 commit 1 — base-model field-level mixin
//! attributes: `auto_uuid`, `auto_now_add`, `auto_now`, `soft_delete`.
//!
//! `auto_now_add` and `auto_now` rely on Postgres' `now()` DEFAULT (so
//! `created_at` works even when `INSERT` skips the column) and on the
//! macro's UPDATE rewrite (so `updated_at` always reflects wall-clock).
//! `auto_uuid` skips the PK column so Postgres' `gen_random_uuid()`
//! DEFAULT fires. `soft_delete` emits `soft_delete_on` / `restore_on`
//! that flip a nullable timestamp column instead of deleting the row.
//!
//! Skipped silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::sql::sqlx;
use rustango::Model;
use tokio::sync::Mutex;

// `Auto<i64>` PK + auto_now_add + auto_now + soft_delete. Mirrors the
// "BaseModel" shape a Django user would inherit.
//
// Mixin fields are wrapped in `Auto<T>` so the existing skip-on-INSERT
// path drops them from the column list and the DB DEFAULT fires.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_mixin_post", display = "title")]
#[allow(dead_code)]
pub struct MixinPost {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(auto_now_add)]
    pub created_at: rustango::sql::Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: rustango::sql::Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

// UUID-PK variant (auto_uuid). Tests the gen_random_uuid() DEFAULT.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_mixin_uuid", display = "name")]
#[allow(dead_code)]
pub struct UuidThing {
    #[rustango(auto_uuid)]
    pub id: rustango::sql::Auto<uuid::Uuid>,
    #[rustango(max_length = 32)]
    pub name: String,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn setup_post(pool: &sqlx::PgPool) {
    let _ = sqlx::query(r#"DROP TABLE IF EXISTS "rustango_mixin_post""#)
        .execute(pool)
        .await;
    sqlx::query(
        r#"CREATE TABLE "rustango_mixin_post" (
              "id" BIGSERIAL PRIMARY KEY,
              "title" TEXT NOT NULL,
              "created_at" TIMESTAMPTZ NOT NULL DEFAULT NOW(),
              "updated_at" TIMESTAMPTZ NOT NULL DEFAULT NOW(),
              "deleted_at" TIMESTAMPTZ NULL
          )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn setup_uuid(pool: &sqlx::PgPool) {
    let _ = sqlx::query(r#"DROP TABLE IF EXISTS "rustango_mixin_uuid""#)
        .execute(pool)
        .await;
    sqlx::query(
        r#"CREATE TABLE "rustango_mixin_uuid" (
              "id" UUID PRIMARY KEY DEFAULT gen_random_uuid(),
              "name" TEXT NOT NULL
          )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn auto_now_add_fills_created_at_via_db_default_on_insert() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    // We deliberately leave `created_at` and `updated_at` as the
    // Default DateTime — the `auto_now_add` / `auto_now` flags should
    // mark these columns as `auto`, so the macro skips them on INSERT
    // and Postgres' `DEFAULT NOW()` fills them.
    let mut row = MixinPost {
        id: rustango::sql::Auto::default(),
        title: "first".into(),
        created_at: rustango::sql::Auto::default(),
        updated_at: rustango::sql::Auto::default(),
        deleted_at: None,
    };
    row.save(&pool).await.unwrap();

    let pk = row.id.get().copied().unwrap();
    let stored: (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(
        r#"SELECT "created_at", "updated_at" FROM "rustango_mixin_post" WHERE "id" = $1"#,
    )
    .bind(pk)
    .fetch_one(&pool)
    .await
    .unwrap();
    let now = chrono::Utc::now();
    assert!(
        (now - stored.0).num_seconds().abs() < 5,
        "created_at not close to NOW: {} vs {}",
        stored.0,
        now
    );
    assert!(
        (now - stored.1).num_seconds().abs() < 5,
        "updated_at not close to NOW: {} vs {}",
        stored.1,
        now
    );
}

#[tokio::test]
async fn auto_now_overrides_updated_at_on_every_update() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut row = MixinPost {
        id: rustango::sql::Auto::default(),
        title: "first".into(),
        created_at: rustango::sql::Auto::default(),
        updated_at: rustango::sql::Auto::default(),
        deleted_at: None,
    };
    row.save(&pool).await.unwrap();
    let pk = row.id.get().copied().unwrap();

    let initial: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        r#"SELECT "updated_at" FROM "rustango_mixin_post" WHERE "id" = $1"#,
    )
    .bind(pk)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Sleep a beat so the wall-clock advances measurably between
    // INSERT (DB now()) and UPDATE (Rust Utc::now()).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    row.title = "second".into();
    // Caller leaves `updated_at` unchanged on the in-memory struct;
    // the macro should rebind it to `chrono::Utc::now()` anyway.
    row.save(&pool).await.unwrap();

    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        r#"SELECT "updated_at" FROM "rustango_mixin_post" WHERE "id" = $1"#,
    )
    .bind(pk)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        after > initial,
        "updated_at should advance on UPDATE; was {initial} now {after}",
    );
}

#[tokio::test]
async fn soft_delete_on_sets_deleted_at_then_restore_on_clears_it() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut row = MixinPost {
        id: rustango::sql::Auto::default(),
        title: "doomed".into(),
        created_at: rustango::sql::Auto::default(),
        updated_at: rustango::sql::Auto::default(),
        deleted_at: None,
    };
    row.save(&pool).await.unwrap();
    let pk = row.id.get().copied().unwrap();

    let n = row.soft_delete_on(&pool).await.unwrap();
    assert_eq!(n, 1, "soft_delete should affect exactly 1 row");
    let after_delete: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        r#"SELECT "deleted_at" FROM "rustango_mixin_post" WHERE "id" = $1"#,
    )
    .bind(pk)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(after_delete.is_some(), "deleted_at should be NOW()");

    row.restore_on(&pool).await.unwrap();
    let after_restore: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        r#"SELECT "deleted_at" FROM "rustango_mixin_post" WHERE "id" = $1"#,
    )
    .bind(pk)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        after_restore.is_none(),
        "deleted_at should be NULL after restore"
    );
}

#[tokio::test]
async fn auto_uuid_skips_pk_on_insert_and_db_fills_it() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_uuid(&pool).await;

    // Auto<Uuid> defaults to Unset — the macro skips the column on
    // INSERT, so `gen_random_uuid()` fires and RETURNING populates
    // `row.id` with the DB-assigned UUID.
    let mut row = UuidThing {
        id: rustango::sql::Auto::default(),
        name: "alpha".into(),
    };
    row.insert(&pool).await.unwrap();
    let assigned = row.id.get().copied().expect("PK populated by RETURNING");
    assert_ne!(assigned, uuid::Uuid::nil(), "DB should have generated a UUID");
}
