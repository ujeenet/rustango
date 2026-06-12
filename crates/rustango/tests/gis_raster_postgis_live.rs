#![cfg(feature = "postgres")]
//! Live Postgres + PostGIS test for the `Raster` column — issue #444
//! (gis.gdal raster support). Requires a PostGIS-enabled Postgres with
//! the `postgis_raster` extension (the `postgis_live` CI job runs
//! `postgis/postgis:16-3.4`); skips when `DATABASE_URL` is unset or the
//! extension isn't installed.
//!
//! Exercises the full round-trip: `CREATE EXTENSION postgis_raster`, a
//! `raster` column, a typed insert (the `Raster` PG `Encode` of raw
//! WKB-raster bytes), typed decode back to a `Raster` + header
//! inspection, and a cross-check that PostGIS reads our encoded bytes as
//! a real raster via `ST_Width` / `ST_Height` / `ST_SRID`.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool, Raster};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "gis_tile_live")]
#[allow(dead_code)]
pub struct Tile {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub name: String,
    pub coverage: Raster,
}

#[tokio::test]
async fn raster_column_round_trips_through_postgis() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping PostGIS raster live test");
        return;
    };
    let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");

    // The raster type lives in the separate `postgis_raster` extension
    // (PostGIS 3.0+). Skip if unavailable rather than fail.
    if sqlx::query("CREATE EXTENSION IF NOT EXISTS postgis")
        .execute(&pg)
        .await
        .is_err()
        || sqlx::query("CREATE EXTENSION IF NOT EXISTS postgis_raster")
            .execute(&pg)
            .await
            .is_err()
    {
        eprintln!("`postgis_raster` unavailable — skipping raster live test");
        return;
    }
    for ddl in [
        "DROP TABLE IF EXISTS gis_tile_live",
        "CREATE TABLE gis_tile_live (\
            id BIGSERIAL PRIMARY KEY, \
            name VARCHAR(40) NOT NULL, \
            coverage raster NOT NULL)",
    ] {
        sqlx::query(ddl).execute(&pg).await.expect(ddl);
    }
    let pool: Pool = pg.clone().into();

    // A 2x3 raster at upper-left (10, 20), scale (1, -1), SRID 4326 — the
    // `ST_MakeEmptyRaster(2,3,10,20,1,-1,0,0,4326)` analog. Typed insert
    // exercises the `Raster` WKB `Encode`; PostGIS rejects malformed bytes.
    let cov = Raster::empty(2, 3, 10.0, 20.0, 1.0, -1.0, 0.0, 0.0, 4326);
    let mut tile = Tile {
        id: Auto::default(),
        name: "tile-a".to_owned(),
        coverage: cov.clone(),
    };
    tile.save_pool(&pool).await.expect("insert Raster");

    // Typed decode — the stored raster round-trips back, header intact.
    let rows: Vec<Tile> = Tile::objects().fetch_pool(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    let got = &rows[0].coverage;
    assert_eq!(got.width(), Some(2), "width round-trips");
    assert_eq!(got.height(), Some(3), "height round-trips");
    assert_eq!(got.srid(), Some(4326), "srid round-trips");
    assert_eq!(got.num_bands(), Some(0), "band count round-trips");
    assert_eq!(got.upper_left_x(), Some(10.0));
    assert_eq!(got.scale_y(), Some(-1.0));

    // Cross-check: PostGIS itself parsed our encoded bytes as a raster.
    let (w, h, srid): (i32, i32, i32) = sqlx::query_as(
        "SELECT ST_Width(coverage), ST_Height(coverage), ST_SRID(coverage) FROM gis_tile_live",
    )
    .fetch_one(&pg)
    .await
    .unwrap();
    assert_eq!(
        (w, h, srid),
        (2, 3, 4326),
        "PostGIS reads our WKB as a raster"
    );

    sqlx::query("DROP TABLE IF EXISTS gis_tile_live")
        .execute(&pg)
        .await
        .ok();
}
