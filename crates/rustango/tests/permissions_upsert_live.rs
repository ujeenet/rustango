#![cfg(feature = "postgres")]
//! Live regression for `permissions.rs` upserts after migrating
//! `grant_role_perm` / `assign_role` / `set_user_perm` from raw sqlx
//! to the ORM's `InsertQuery` + `ConflictClause` IR (ORM roadmap P1).
//!
//! Each test: call the function twice with the same args, assert it's
//! idempotent (no DB error), and inspect the row count to confirm the
//! ON CONFLICT clause did the right thing. `set_user_perm` additionally
//! checks that the second call with `granted = false` flips the
//! existing row instead of inserting a duplicate.

#![cfg(feature = "tenancy")]
// The legacy `ensure_tables(&PgPool)` is intentionally exercised
// below — these tests cover the upsert behaviour, not the migration
// path that superseded it.
#![allow(deprecated)]

use rustango::core::Column as _;
use rustango::sql::sqlx;
use rustango::sql::{Auto, Fetcher};
use rustango::tenancy::permissions::{
    assign_role, get_or_create_role, grant_role_perm, set_user_perm, RolePermission,
    UserPermission, UserRole,
};
use rustango::tenancy::User;

use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets the shared PG
/// schema; under cargo's default parallel harness two tests would race
/// on PG's `pg_type_typname_nsp_index` / `pg_class_relname_nsp_index`
/// system-catalog uniques when both try to CREATE/DROP the same table
/// at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    rustango::migrate::drop_all(pool).await.unwrap();
    rustango::migrate::apply_all(pool).await.unwrap();
    // The Model-derived auto-DDL in apply_all creates the four
    // permission tables WITHOUT the composite UNIQUE constraints
    // declared in `permissions::ENSURE_SQL` (the constraints aren't
    // currently on the Model defs as `unique_together`). For these
    // tests to exercise ON CONFLICT we need those constraints, so
    // drop the tables and let `ensure_tables` re-create them with
    // the constraints. Production deployments hit this same code
    // path because `ensure_tables` runs before any inventory-driven
    // CREATE TABLE for these model types.
    for t in [
        "rustango_user_permissions",
        "rustango_user_roles",
        "rustango_role_permissions",
        "rustango_roles",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    rustango::tenancy::ensure_permission_tables(pool)
        .await
        .unwrap();
}

async fn make_user(pool: &sqlx::PgPool, username_prefix: &str) -> i64 {
    let username = format!(
        "{username_prefix}_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let mut user = User {
        id: Auto::default(),
        username,
        password_hash: "test-hash".to_owned(),
        is_superuser: false,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
    };
    user.insert(pool).await.unwrap();
    *user.id.get().expect("PK assigned by RETURNING")
}

#[tokio::test]
async fn grant_role_perm_is_idempotent_via_on_conflict_do_nothing() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let role_id = get_or_create_role("test-role", "test", &pool)
        .await
        .unwrap();
    grant_role_perm(role_id, "post.add", &pool).await.unwrap();
    // Second grant: must be a no-op, not an error. This is the
    // race-safety guarantee we get from ON CONFLICT DO NOTHING — if we
    // had used a fetch-then-insert pattern instead, two concurrent
    // grants could both observe "absent" and both INSERT.
    grant_role_perm(role_id, "post.add", &pool).await.unwrap();

    let rows: Vec<RolePermission> = RolePermission::objects()
        .where_(RolePermission::role_id.eq(role_id))
        .where_(RolePermission::codename.eq("post.add"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "two grants of the same codename should leave one row"
    );

    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn assign_role_is_idempotent_via_on_conflict_do_nothing() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let role_id = get_or_create_role("seat", "test", &pool).await.unwrap();
    let user_id = make_user(&pool, "perm_test_assign").await;

    assign_role(user_id, role_id, &pool).await.unwrap();
    assign_role(user_id, role_id, &pool).await.unwrap();

    let rows: Vec<UserRole> = UserRole::objects()
        .where_(UserRole::user_id.eq(user_id))
        .where_(UserRole::role_id.eq(role_id))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "two assigns should leave one row");

    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn set_user_perm_flips_existing_row_via_on_conflict_do_update() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let user_id = make_user(&pool, "perm_test_setperm").await;
    let codename = "post.delete";

    // Initial grant.
    set_user_perm(user_id, codename, true, &pool).await.unwrap();
    let rows: Vec<UserPermission> = UserPermission::objects()
        .where_(UserPermission::user_id.eq(user_id))
        .where_(UserPermission::codename.eq(codename))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "first call should insert one row");
    assert!(rows[0].granted, "initial grant should be true");

    // Flip to denial — must UPDATE the same row, not insert a second
    // one. Pre-migration this used `ON CONFLICT (user_id, codename)
    // DO UPDATE SET granted = EXCLUDED.granted` in raw SQL; the IR
    // path emits the same shape via ConflictClause::DoUpdate.
    set_user_perm(user_id, codename, false, &pool)
        .await
        .unwrap();
    let rows: Vec<UserPermission> = UserPermission::objects()
        .where_(UserPermission::user_id.eq(user_id))
        .where_(UserPermission::codename.eq(codename))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "second call should NOT insert a duplicate");
    assert!(!rows[0].granted, "second call should flip granted to false");

    rustango::migrate::drop_all(&pool).await.unwrap();
}
