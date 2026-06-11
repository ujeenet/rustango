#![cfg(feature = "postgres")]
//! Live Postgres + pgvector test for the vector column + k-NN search —
//! issue #824. Requires a pgvector-enabled Postgres (the CI / local
//! `docker-compose` Postgres is `pgvector/pgvector:pg16`); skips when
//! `DATABASE_URL` is unset.
//!
//! Exercises the full round-trip: `CREATE EXTENSION vector`, a
//! `vector(3)` column migrated implicitly via raw DDL, typed inserts
//! (the `Vector` PG binary `Encode`), a k-NN query ordered by L2
//! distance, and typed decode of the stored embedding back to `Vec<f32>`.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool, Vector};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vec_doc_live")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub title: String,
    #[rustango(vector(dims = 3))]
    pub embedding: Vector,
}

#[tokio::test]
async fn vector_column_round_trip_and_knn() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping pgvector live test");
        return;
    };
    let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");

    // pgvector must be enabled; the test image (pgvector/pgvector:pg16)
    // ships it. If it's somehow unavailable, skip rather than fail so a
    // non-pgvector PG doesn't break the suite.
    if sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(&pg)
        .await
        .is_err()
    {
        eprintln!("`vector` extension unavailable — skipping pgvector live test");
        return;
    }
    for ddl in [
        "DROP TABLE IF EXISTS vec_doc_live",
        "CREATE TABLE vec_doc_live (id BIGSERIAL PRIMARY KEY, title VARCHAR(40) NOT NULL, embedding vector(3) NOT NULL)",
    ] {
        sqlx::query(ddl).execute(&pg).await.expect(ddl);
    }
    let pool: Pool = pg.into();

    // Typed inserts — exercises the `Vector` binary `Encode`.
    for (title, emb) in [
        ("A", vec![1.0_f32, 0.0, 0.0]),
        ("B", vec![0.8, 0.2, 0.0]),
        ("C", vec![0.0, 1.0, 0.0]),
    ] {
        let mut d = Doc {
            id: Auto::default(),
            title: title.to_owned(),
            embedding: Vector::new(emb),
        };
        d.save_pool(&pool).await.unwrap();
    }

    // k-NN: the 2 nearest to [1,0,0] by L2 distance are A (dist 0) then B.
    use rustango::core::VectorMetric;
    let nearest: Vec<String> = Doc::objects()
        .k_nearest("embedding", vec![1.0, 0.0, 0.0], 2, VectorMetric::L2)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.title)
        .collect();
    assert_eq!(nearest, vec!["A", "B"], "k-NN order by L2 distance");

    // Full ordering across all three, + typed decode of the embedding.
    let all: Vec<Doc> = Doc::objects()
        .order_by_distance("embedding", vec![1.0, 0.0, 0.0], VectorMetric::L2)
        .fetch_pool(&pool)
        .await
        .unwrap();
    let order: Vec<&str> = all.iter().map(|d| d.title.as_str()).collect();
    assert_eq!(order, vec!["A", "B", "C"], "full distance ordering");
    // The stored embedding decodes back to the inserted `Vec<f32>`.
    assert_eq!(
        all[0].embedding.0,
        vec![1.0, 0.0, 0.0],
        "A decodes its vector"
    );

    // Cosine ordering ranks A first too (identical direction → 0 distance).
    let cos_first = Doc::objects()
        .k_nearest("embedding", vec![1.0, 0.0, 0.0], 1, VectorMetric::Cosine)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(cos_first[0].title, "A");
}
