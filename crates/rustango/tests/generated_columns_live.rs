#![cfg(feature = "postgres")]
//! `#[rustango(generated_as = "EXPR")]` — closes future-backlog
//! item #35 ("computed/virtual columns"). The macro skips the column
//! from every INSERT/UPDATE path and the DDL writer emits
//! `GENERATED ALWAYS AS (<expr>) STORED`, so Postgres recomputes the
//! value on every write.

#![cfg(feature = "tenancy")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{sqlx, Auto, Fetcher};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "_gen_invoice")]
pub struct Invoice {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub price: f64,
    pub quantity: i32,
    /// Database-computed: the model carries `0.0` at construction
    /// time, but every INSERT and UPDATE leaves the column out and
    /// Postgres computes `price * quantity` server-side.
    #[rustango(generated_as = "price * quantity::double precision")]
    pub total: f64,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    rustango::migrate::drop_all(pool).await.unwrap();
    rustango::migrate::apply_all(pool).await.unwrap();
}

#[tokio::test]
async fn generated_column_is_computed_on_insert() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // Construct with a stale placeholder for `total` — the macro
    // must drop it from INSERT so Postgres' GENERATED expression
    // produces the real value.
    let mut inv = Invoice {
        id: Auto::default(),
        price: 9.99,
        quantity: 4,
        total: 0.0,
    };
    inv.insert(&pool).await.unwrap();
    let id = *inv.id.get().expect("PK assigned by RETURNING");

    // Read back through the ORM — the FromRow path decodes the
    // GENERATED column normally, so `total` should be 9.99 * 4.
    let rows: Vec<Invoice> = Invoice::objects()
        .where_(Invoice::id.eq(id))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let approx = (rows[0].total - (9.99 * 4.0)).abs() < 1e-6;
    assert!(
        approx,
        "expected total ≈ {expected}, got {actual}",
        expected = 9.99 * 4.0,
        actual = rows[0].total,
    );

    // Pure smoke: emitting `total` from the Rust struct must NOT
    // override the DB value. Construct an instance with a
    // wildly-wrong `total`, INSERT, fetch back — the value should
    // still match price * quantity.
    let mut wrong = Invoice {
        id: Auto::default(),
        price: 2.0,
        quantity: 10,
        total: 99_999.0, // bogus
    };
    wrong.insert(&pool).await.unwrap();
    let wrong_id = *wrong.id.get().unwrap();
    let rows: Vec<Invoice> = Invoice::objects()
        .where_(Invoice::id.eq(wrong_id))
        .fetch(&pool)
        .await
        .unwrap();
    let approx = (rows[0].total - 20.0).abs() < 1e-6;
    assert!(
        approx,
        "Postgres must ignore the Rust-side `total` value and recompute: got {}",
        rows[0].total
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "_gen_invoice" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn generated_column_is_recomputed_on_update() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut inv = Invoice {
        id: Auto::default(),
        price: 5.0,
        quantity: 2,
        total: 0.0,
    };
    inv.insert(&pool).await.unwrap();
    let id = *inv.id.get().unwrap();

    // Mutate price + quantity, save. The generated column should
    // stay out of the UPDATE statement entirely; Postgres
    // recomputes after the UPDATE applies.
    inv.price = 7.5;
    inv.quantity = 4;
    inv.total = 12345.0; // bogus again
    inv.save(&pool).await.unwrap();

    let rows: Vec<Invoice> = Invoice::objects()
        .where_(Invoice::id.eq(id))
        .fetch(&pool)
        .await
        .unwrap();
    let approx = (rows[0].total - 30.0).abs() < 1e-6;
    assert!(
        approx,
        "after UPDATE the generated column should reflect 7.5 * 4 = 30.0, got {}",
        rows[0].total
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "_gen_invoice" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
