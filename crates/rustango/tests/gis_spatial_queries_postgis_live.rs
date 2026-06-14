#![cfg(feature = "postgres")]
//! Live Postgres + PostGIS test for the spatial query layer — issue #58
//! (GeoDjango queries/functions), the follow-up to the #443 geometry
//! type. Requires a PostGIS-enabled Postgres (the `postgis_live` CI job);
//! skips when `DATABASE_URL` is unset or `postgis` isn't installed.
//!
//! Exercises the ergonomic `QuerySet` helpers end-to-end against a real
//! PostGIS: `order_by_distance_to` (ST_Distance nearest-first ordering)
//! and `filter_dwithin` (ST_DWithin "within radius" predicate), plus a
//! topological predicate via `where_raw` + the `st_intersects` builder.

use rustango::core::funcs::st_intersects;
use rustango::core::{Expr, Op, SqlValue, WhereExpr};
use rustango::sql::{sqlx, Auto, FetcherPool as _, Point, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "gis_spatial_live")]
#[allow(dead_code)]
pub struct Place {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub name: String,
    #[rustango(geometry(srid = 4326))]
    pub location: Point,
}

#[tokio::test]
async fn spatial_distance_dwithin_and_intersects() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping PostGIS spatial live test");
        return;
    };
    let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");
    if sqlx::query("CREATE EXTENSION IF NOT EXISTS postgis")
        .execute(&pg)
        .await
        .is_err()
    {
        eprintln!("`postgis` extension unavailable — skipping spatial live test");
        return;
    }
    for ddl in [
        "DROP TABLE IF EXISTS gis_spatial_live",
        "CREATE TABLE gis_spatial_live (\
            id BIGSERIAL PRIMARY KEY, \
            name VARCHAR(40) NOT NULL, \
            location geometry(Point,4326) NOT NULL)",
    ] {
        sqlx::query(ddl).execute(&pg).await.expect(ddl);
    }
    let pool: Pool = pg.clone().into();

    // Three places along the prime meridian at increasing distance from
    // the origin (0,0): near (0.0), mid (0.5°), far (5.0°).
    for (name, lon) in [("near", 0.0), ("mid", 0.5), ("far", 5.0)] {
        let mut place = Place {
            id: Auto::default(),
            name: name.to_owned(),
            location: Point::with_srid(lon, 0.0, 4326),
        };
        place.save_pool(&pool).await.expect("insert place");
    }
    let origin = Point::with_srid(0.0, 0.0, 4326);

    // order_by_distance_to → ST_Distance ascending: nearest first.
    let ordered: Vec<String> = Place::objects()
        .order_by_distance_to("location", origin)
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(
        ordered,
        vec!["near", "mid", "far"],
        "ST_Distance orders nearest-first"
    );

    // filter_dwithin(1.0°) → only `near` (0.0) and `mid` (0.5) qualify;
    // `far` (5.0) is excluded.
    let within: Vec<String> = Place::objects()
        .filter_dwithin("location", origin, 1.0)
        .order_by_distance_to("location", origin)
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(within, vec!["near", "mid"], "ST_DWithin filters by radius");

    // Topological predicate via where_raw + st_intersects: only the row
    // whose geometry intersects the exact `mid` point (0.5, 0.0).
    let mid = Point::with_srid(0.5, 0.0, 4326);
    let hits: Vec<String> = Place::objects()
        .where_raw(WhereExpr::ExprCompare {
            lhs: st_intersects(Expr::Column("location"), mid),
            op: Op::Eq,
            rhs: Expr::Literal(SqlValue::Bool(true)),
        })
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(hits, vec!["mid"], "ST_Intersects matches the exact point");

    sqlx::query("DROP TABLE IF EXISTS gis_spatial_live")
        .execute(&pg)
        .await
        .ok();
}
