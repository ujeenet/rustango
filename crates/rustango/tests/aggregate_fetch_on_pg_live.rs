//! `AggregateBuilder::fetch_on` / `ValuesQuerySet::fetch_on` — tenant-scoped
//! aggregates (#1172).
//!
//! In schema-per-tenant Postgres the tenant is selected by `SET search_path`
//! on the **checked-out connection**, so a terminal that resolves against the
//! pool reads `public` instead. `QuerySet` has had `fetch_on` for this; the
//! aggregate/values builders did not, leaving a tenant app to drop to raw SQL.
//!
//! These tests put *different data* in two schemas and assert the aggregate
//! reads the connection's schema — a terminal that silently used the pool
//! would return the other schema's numbers and pass a weaker test.
//!
//! Reads `DATABASE_URL`; skips silently when unset.

#![cfg(feature = "postgres")]

use rustango::core::aggregates::sum;
use rustango::core::Model as _;
use rustango::sql::sqlx::{self, PgPool};
use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "agg_on_setlog")]
#[rustango(app = "agg_on_app")]
pub struct SetLog {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub member_id: i64,
    pub volume: i64,
}

const TENANT: &str = "agg_on_tenant";

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPool::connect(&url).await.ok()
}

/// Same table in two schemas, different numbers in each.
async fn setup(pool: &PgPool) {
    for (schema, volumes) in [("public", [10_i64, 5]), (TENANT, [100, 50])] {
        if schema != "public" {
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
                .execute(pool)
                .await
                .unwrap();
        }
        sqlx::query(&format!("DROP TABLE IF EXISTS {schema}.agg_on_setlog"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(&format!(
            "CREATE TABLE {schema}.agg_on_setlog \
             (id BIGSERIAL PRIMARY KEY, member_id BIGINT NOT NULL, volume BIGINT NOT NULL)"
        ))
        .execute(pool)
        .await
        .unwrap();
        for v in volumes {
            sqlx::query(&format!(
                "INSERT INTO {schema}.agg_on_setlog (member_id, volume) VALUES (1, {v})"
            ))
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

/// The headline case from the issue: total volume per member, grouped, run on
/// a tenant-scoped connection. Must read the tenant's rows (100+50), not
/// `public`'s (10+5).
#[tokio::test]
async fn aggregate_fetch_on_reads_the_connections_schema() {
    let Some(pool) = pool().await else {
        eprintln!("skipping — set DATABASE_URL");
        return;
    };
    setup(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    sqlx::query(&format!("SET search_path TO {TENANT}, public"))
        .execute(&mut *conn)
        .await
        .unwrap();

    let rows = SetLog::objects()
        .values(&["member_id"])
        .annotate("total", sum("volume").into())
        .fetch_on(&mut *conn)
        .await
        .expect("aggregate must run on the borrowed tenant connection");

    assert_eq!(rows.len(), 1, "one group (member_id = 1)");
    let total = format!("{:?}", rows[0].get("total"));
    assert!(
        total.contains("150"),
        "must sum the TENANT schema's rows (100+50=150), got {total} — \
         a pool-resolved aggregate would have returned 15"
    );
}

/// Same guarantee for the plain values projection.
#[tokio::test]
async fn values_fetch_on_reads_the_connections_schema() {
    let Some(pool) = pool().await else {
        eprintln!("skipping — set DATABASE_URL");
        return;
    };
    setup(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    sqlx::query(&format!("SET search_path TO {TENANT}, public"))
        .execute(&mut *conn)
        .await
        .unwrap();

    let rows = SetLog::objects()
        .values_dict(&["volume"])
        .fetch_on(&mut *conn)
        .await
        .expect("values projection must run on the borrowed connection");

    assert_eq!(rows.len(), 2, "the tenant schema holds two rows");
    let dumped = format!("{rows:?}");
    assert!(
        dumped.contains("100") && dumped.contains("50"),
        "must be the tenant's volumes, got {dumped}"
    );
}
