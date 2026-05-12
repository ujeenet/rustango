#![cfg(feature = "postgres")]
//! Verifies that the `#[derive(Model)]` macro's generated `upsert()`
//! picks the first `unique_together` group as its `ON CONFLICT` target
//! when one is declared on the model — instead of always defaulting to
//! the (`Auto<T>`) primary key.
//!
//! The motivating case is `RolePermission` / `UserRole` / `UserPermission`
//! in the tenancy permission engine: their PK is `BIGSERIAL` (never
//! collides) and the meaningful uniqueness is a composite (`role_id`,
//! `codename`) constraint. Pre-fix, `RolePermission::upsert(&pool)`
//! silently inserted duplicates. Post-fix, the macro emits
//! `ON CONFLICT (role_id, codename) DO UPDATE SET …`.

#![cfg(feature = "tenancy")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::sqlx;
use rustango::sql::{Auto, Fetcher};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "_upsert_uq_demo", unique_together = "team_id, codename")]
pub struct DemoMembership {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub team_id: i64,
    #[rustango(max_length = 64)]
    pub codename: String,
    #[rustango(max_length = 200)]
    pub note: String,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "_upsert_uq_demo" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "_upsert_uq_demo" (
            "id"        BIGSERIAL    PRIMARY KEY,
            "team_id"   BIGINT       NOT NULL,
            "codename"  VARCHAR(64)  NOT NULL,
            "note"      VARCHAR(200) NOT NULL DEFAULT '',
            CONSTRAINT "_upsert_uq_demo_uq" UNIQUE ("team_id", "codename")
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn upsert_uses_unique_together_as_conflict_target() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // First upsert — pure insert.
    let mut first = DemoMembership {
        id: Auto::default(),
        team_id: 7,
        codename: "manage".into(),
        note: "initial".into(),
    };
    first.upsert(&pool).await.unwrap();
    let id_first = *first.id.get().expect("PK assigned by RETURNING");

    // Second upsert with the SAME (team_id, codename) but a fresh
    // Auto::default() id. Pre-fix this would have inserted a second
    // row (PK never conflicts on a BIGSERIAL); post-fix it conflicts
    // on the unique_together and updates the existing row's `note`
    // via DO UPDATE SET note = EXCLUDED.note.
    let mut second = DemoMembership {
        id: Auto::default(),
        team_id: 7,
        codename: "manage".into(),
        note: "updated".into(),
    };
    second.upsert(&pool).await.unwrap();

    let rows: Vec<DemoMembership> = DemoMembership::objects()
        .where_(DemoMembership::team_id.eq(7_i64))
        .where_(DemoMembership::codename.eq("manage"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "second upsert should NOT insert a duplicate");
    assert_eq!(
        rows[0].note, "updated",
        "the second upsert should overwrite `note` via DO UPDATE"
    );
    // The id stays the original one — DO UPDATE doesn't allocate a
    // new BIGSERIAL.
    assert_eq!(
        *rows[0].id.get().expect("PK loaded"),
        id_first,
        "the surviving row keeps the original auto-PK"
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "_upsert_uq_demo" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
