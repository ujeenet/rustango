#![cfg(all(feature = "casts", feature = "postgres"))]
//! Live PostgreSQL round-trip for the `EncryptedString` cast (#819) —
//! same coverage as the SQLite test against PG, so the encrypt/decrypt +
//! TEXT-column path is exercised on a second backend.
//!
//! Skips silently when `DATABASE_URL` is unset (runs in CI's
//! `postgres_test` job).

use std::sync::OnceLock;

use rustango::casts::{Cast, EncryptedString};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

fn suite_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cast_pg_patient")]
#[allow(dead_code)]
pub struct Patient {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub ssn: Cast<EncryptedString>,
}

async fn pool() -> Option<Pool> {
    std::env::set_var("RUSTANGO_SECRET_KEY", "pg-live-secret");
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool: Pool = sqlx::PgPool::connect(&url).await.ok()?.into();
    let pg = pool.as_postgres().unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "cast_pg_patient" CASCADE"#)
        .execute(pg)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "cast_pg_patient" ("id" BIGSERIAL PRIMARY KEY, "ssn" TEXT NOT NULL)"#,
    )
    .execute(pg)
    .await
    .unwrap();
    Some(pool)
}

#[tokio::test]
async fn encrypted_cast_round_trips_on_pg() {
    let _g = suite_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let plaintext = "top-secret-value";

    let mut patient = Patient {
        id: Auto::default(),
        ssn: Cast::new(plaintext.to_owned()),
    };
    patient.save_pool(&pool).await.unwrap();
    let id = *patient.id.get().unwrap();

    let fetched = Patient::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .expect("row present");
    assert_eq!(&*fetched.ssn, plaintext);

    // Raw column holds ciphertext.
    let pg = pool.as_postgres().unwrap();
    let raw: (String,) = sqlx::query_as(r#"SELECT "ssn" FROM "cast_pg_patient" WHERE "id" = $1"#)
        .bind(id)
        .fetch_one(pg)
        .await
        .unwrap();
    assert_ne!(raw.0, plaintext);
}
