#![cfg(feature = "postgres")]
//! Live Postgres + PostGIS test for the `Point` geometry column — issue
//! #443 (gis.geos geometry types). Requires a PostGIS-enabled Postgres
//! (the `postgis_live` CI job runs `postgis/postgis:16-3.4`); skips when
//! `DATABASE_URL` is unset or the `postgis` extension isn't installed
//! (e.g. the plain `postgres_test` pgvector image), so it never breaks a
//! non-PostGIS suite.
//!
//! Exercises the full round-trip: `CREATE EXTENSION postgis`, a
//! `geometry(Point, 4326)` column, a typed insert (the `Point` PG EWKB
//! `Encode`), typed decode back to a `Point`, and a cross-check that
//! PostGIS itself reads our encoded bytes as a real `POINT(...)` via
//! `ST_AsText` / `ST_Distance`.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Point, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "gis_place_live")]
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
async fn point_column_round_trips_through_postgis() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping PostGIS live test");
        return;
    };
    let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");

    // PostGIS must be enabled; the `postgis_live` image ships it. If it's
    // unavailable (e.g. the plain pgvector test image), skip rather than
    // fail so a non-PostGIS PG doesn't break the suite.
    if sqlx::query("CREATE EXTENSION IF NOT EXISTS postgis")
        .execute(&pg)
        .await
        .is_err()
    {
        eprintln!("`postgis` extension unavailable — skipping PostGIS live test");
        return;
    }
    for ddl in [
        "DROP TABLE IF EXISTS gis_place_live",
        "CREATE TABLE gis_place_live (\
            id BIGSERIAL PRIMARY KEY, \
            name VARCHAR(40) NOT NULL, \
            location geometry(Point,4326) NOT NULL)",
    ] {
        sqlx::query(ddl).execute(&pg).await.expect(ddl);
    }
    let pool: Pool = pg.clone().into();

    // Typed insert — exercises the `Point` EWKB binary `Encode`. The
    // column is constrained to SRID 4326, so PostGIS rejects the insert
    // outright if our encoded bytes are malformed or carry a wrong SRID.
    let cn_tower = Point::with_srid(-79.387_054, 43.642_566, 4326);
    let mut p = Place {
        id: Auto::default(),
        name: "CN Tower".to_owned(),
        location: cn_tower,
    };
    p.save_pool(&pool).await.expect("insert Point");

    // Typed decode — the stored geometry round-trips back to a `Point`.
    let rows: Vec<Place> = Place::objects().fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    let got = rows[0].location;
    assert_eq!(got.srid, 4326);
    assert!(
        (got.x - cn_tower.x).abs() < 1e-9,
        "x round-trips: {}",
        got.x
    );
    assert!(
        (got.y - cn_tower.y).abs() < 1e-9,
        "y round-trips: {}",
        got.y
    );

    // Cross-check: PostGIS itself parsed our encoded bytes as a real
    // Point — `ST_AsText` reflects the coordinates back.
    let wkt: String = sqlx::query_scalar("SELECT ST_AsText(location) FROM gis_place_live")
        .fetch_one(&pg)
        .await
        .unwrap();
    assert!(
        wkt.starts_with("POINT(-79.387054 43.642566"),
        "PostGIS reads our EWKB as a Point: {wkt}"
    );

    // And a spatial function works against the stored geometry, proving
    // it's a usable PostGIS value (the broader ST_* query DSL is #58).
    let dist_km: f64 = sqlx::query_scalar(
        "SELECT ST_Distance(location::geography, \
         ST_SetSRID(ST_MakePoint(-79.3871, 43.6426), 4326)::geography) / 1000.0 \
         FROM gis_place_live",
    )
    .fetch_one(&pg)
    .await
    .unwrap();
    assert!(
        dist_km < 0.1,
        "stored point is ~0 km from itself: {dist_km}"
    );

    sqlx::query("DROP TABLE IF EXISTS gis_place_live")
        .execute(&pg)
        .await
        .ok();
}
