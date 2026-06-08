#![cfg(all(feature = "casts", feature = "sqlite"))]
//! Live SQLite round-trip for the `EncryptedString` attribute cast
//! (#819): a `Cast<EncryptedString>` field encrypts at rest on INSERT
//! and decrypts on SELECT, while the raw column holds ciphertext.

use rustango::casts::{Cast, EncryptedString};
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "cast_patient")]
#[allow(dead_code)]
pub struct Patient {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    pub ssn: Cast<EncryptedString>,
}

async fn make_pool() -> Pool {
    // Key required for the encrypted cast.
    std::env::set_var("RUSTANGO_SECRET_KEY", "sqlite-live-secret");
    let p = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE cast_patient (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, ssn TEXT NOT NULL)",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

#[tokio::test]
async fn encrypted_cast_round_trips_and_stores_ciphertext() {
    let pool = make_pool().await;
    let plaintext = "123-45-6789";

    let mut patient = Patient {
        id: Auto::default(),
        name: "Ada".into(),
        ssn: Cast::new(plaintext.to_owned()),
    };
    patient.save_pool(&pool).await.unwrap();
    let id = *patient.id.get().unwrap();

    // SELECT through the model → decrypts transparently.
    let fetched = Patient::objects()
        .filter("id", id)
        .first(&pool)
        .await
        .unwrap()
        .expect("row present");
    assert_eq!(&*fetched.ssn, plaintext);

    // Raw column holds ciphertext, not the plaintext.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let raw: (String,) = sqlx::query_as("SELECT ssn FROM cast_patient WHERE id = ?")
        .bind(id)
        .fetch_one(sq)
        .await
        .unwrap();
    assert_ne!(
        raw.0, plaintext,
        "column must store ciphertext, not plaintext"
    );
    assert!(
        raw.0.len() > plaintext.len(),
        "ciphertext carries nonce + tag"
    );
}
