#![cfg(feature = "postgres")]
#![allow(irrefutable_let_patterns)] // Pool enum is single-variant under postgres-only builds.
//! Live PG end-to-end test for `Model::save_pool_fields` (issue #66).
//! Confirms the UPDATE narrows correctly against a real Postgres
//! backend and that an `Auto<T>`-PK model behaves the same.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "spfl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
    pub views: i64,
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    Some(Pool::Postgres(pg))
}

async fn fresh(pool: &Pool) {
    if let Pool::Postgres(pg) = pool {
        sqlx::query(r#"DROP TABLE IF EXISTS "spfl_post" CASCADE"#)
            .execute(pg)
            .await
            .unwrap();
        sqlx::query(
            r#"CREATE TABLE "spfl_post" (
                "id" BIGSERIAL PRIMARY KEY,
                "title" VARCHAR(200) NOT NULL,
                "status" VARCHAR(20) NOT NULL,
                "views" BIGINT NOT NULL
            )"#,
        )
        .execute(pg)
        .await
        .unwrap();
    }
}

async fn cleanup(pool: &Pool) {
    if let Pool::Postgres(pg) = pool {
        sqlx::query(r#"DROP TABLE IF EXISTS "spfl_post" CASCADE"#)
            .execute(pg)
            .await
            .unwrap();
    }
}

/// Insert a row, then `save_pool_fields(["title"])` and confirm the
/// other columns survived even though we mutated them in memory.
#[tokio::test]
async fn save_pool_fields_narrows_update_on_pg() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let mut row = Post {
        id: Auto::Unset,
        title: "orig".into(),
        status: "draft".into(),
        views: 0,
    };
    row.insert_pool(&pool).await.unwrap();
    let pk = *row.id.get().unwrap();

    // Mutate every column in memory; ask save_pool_fields to write
    // ONLY `title`. The DB row should reflect the title change but
    // keep status='draft' / views=0.
    row.title = "rewritten".into();
    row.status = "would-be-overwritten".into();
    row.views = 999;
    row.save_pool_fields(&["title"], &pool).await.unwrap();

    if let Pool::Postgres(pg) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as(r#"SELECT "title", "status", "views" FROM "spfl_post" WHERE "id" = $1"#)
                .bind(pk)
                .fetch_one(pg)
                .await
                .unwrap();
        assert_eq!(title, "rewritten");
        assert_eq!(status, "draft", "status not listed → DB value preserved");
        assert_eq!(views, 0, "views not listed → DB value preserved");
    }

    cleanup(&pool).await;
}

/// Two-writer divergence: A and B both read the original, mutate
/// different fields, and both call `save_pool_fields` on their
/// single field. Result should carry BOTH changes — no lost-update.
#[tokio::test]
async fn two_writers_preserve_each_others_changes_on_pg() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let mut a = Post {
        id: Auto::Unset,
        title: "orig".into(),
        status: "draft".into(),
        views: 0,
    };
    a.insert_pool(&pool).await.unwrap();
    let pk = *a.id.get().unwrap();

    let mut b = Post {
        id: Auto::Set(pk),
        title: "orig".into(),
        status: "draft".into(),
        views: 0,
    };

    // A flips title.
    a.title = "from-A".into();
    a.save_pool_fields(&["title"], &pool).await.unwrap();

    // B flips status. B's `title` is stale ("orig"), but it's not in
    // the update_fields list, so A's title write survives.
    b.status = "from-B".into();
    b.save_pool_fields(&["status"], &pool).await.unwrap();

    if let Pool::Postgres(pg) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as(r#"SELECT "title", "status", "views" FROM "spfl_post" WHERE "id" = $1"#)
                .bind(pk)
                .fetch_one(pg)
                .await
                .unwrap();
        assert_eq!(title, "from-A", "A's title write must survive");
        assert_eq!(status, "from-B", "B's status write must survive");
        assert_eq!(views, 0);
    }

    cleanup(&pool).await;
}
