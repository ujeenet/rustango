//! `Point` — PostGIS `geometry(Point, SRID)` column wrapper (GeoDjango
//! `gis.geos` geometry types, issue #443).
//!
//! Declare a `geometry(Point, …)` column on a model and round-trip it as
//! a Rust `Point`:
//!
//! ```ignore
//! #[derive(Model)]
//! #[rustango(table = "place")]
//! struct Place {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,
//!     // → DDL `location geometry(Point, 4326)`; round-trips as a `Point`.
//!     #[rustango(geometry(srid = 4326))]
//!     location: Point,
//! }
//! ```
//!
//! ## PostgreSQL/PostGIS only, by language semantics
//!
//! `geometry` is a PostGIS extension type. Like `vector` / trigram /
//! full-text / `Array<T>`, `Point` is **PG-only by language semantics**:
//! the migration writer emits a degraded `TEXT` column on MySQL / SQLite,
//! and the [`sqlx::Decode`] path raises a clear error on those backends
//! rather than silently mis-storing. The type still *compiles* under
//! every backend (the per-backend [`sqlx::Type`] / [`sqlx::Decode`] impls
//! below are total), so the `sqlite,tenancy` litmus build keeps passing.
//!
//! Spatial *queries* (`ST_Distance` / `ST_DWithin` / `ST_Contains` …)
//! are the separate GeoDjango query layer (issue #58); this issue (#443)
//! is the geometry *type* + storage + DDL.
//!
//! ## Why a newtype rather than a bare `(f64, f64)`
//!
//! Same rationale as [`crate::sql::Vector`]: `#[derive(Model)]` emits one
//! shared `FromRow` body reused across the PG / MySQL / SQLite decoders,
//! so the column type needs total `Decode` / `Type` impls for all three
//! backends (a real PostGIS EWKB codec on PG, erroring stubs elsewhere).

/// The default spatial reference identifier — WGS 84 (GPS lat/long),
/// PostGIS's most common SRID and GeoDjango's default.
pub const SRID_WGS84: u32 = 4326;

/// A 2-D point in a PostGIS `geometry(Point, SRID)` column — see the
/// [module docs](self). Carries its SRID so it round-trips losslessly
/// through the EWKB wire format.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// X coordinate (longitude for SRID 4326).
    pub x: f64,
    /// Y coordinate (latitude for SRID 4326).
    pub y: f64,
    /// Spatial reference identifier (4326 = WGS 84).
    pub srid: u32,
}

impl Point {
    /// A point in WGS 84 (SRID 4326) — the GPS lat/long default. `x` is
    /// longitude, `y` is latitude.
    #[must_use]
    pub fn new(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            srid: SRID_WGS84,
        }
    }

    /// A point in an explicit spatial reference system.
    #[must_use]
    pub fn with_srid(x: f64, y: f64, srid: u32) -> Self {
        Self { x, y, srid }
    }

    /// `POINT(x y)` well-known text — handy for diagnostics and the text
    /// decode fallback. (Note: WKT carries no SRID; EWKT would prefix
    /// `SRID=…;`.)
    #[must_use]
    pub fn to_wkt(&self) -> String {
        format!("POINT({} {})", self.x, self.y)
    }
}

impl Default for Point {
    fn default() -> Self {
        Self::new(0.0, 0.0)
    }
}

// ---- `Point` → `SqlValue` (INSERT / UPDATE bind, via SqlValue::Geometry) ----

impl From<Point> for crate::core::SqlValue {
    fn from(p: Point) -> Self {
        crate::core::SqlValue::Geometry {
            x: p.x,
            y: p.y,
            srid: p.srid,
        }
    }
}

// A `Point` literal in an expression tree — lets the spatial function
// builders (`st_distance(F("loc"), point)`, #58) accept a bare `Point`.
impl From<Point> for crate::core::Expr {
    fn from(p: Point) -> Self {
        crate::core::Expr::Literal(p.into())
    }
}

// ---- serde: a plain `{ "x", "y", "srid" }` object -------------------

impl serde::Serialize for Point {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut s = serializer.serialize_struct("Point", 3)?;
        s.serialize_field("x", &self.x)?;
        s.serialize_field("y", &self.y)?;
        s.serialize_field("srid", &self.srid)?;
        s.end()
    }
}

impl<'de> serde::Deserialize<'de> for Point {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            x: f64,
            y: f64,
            #[serde(default = "default_srid")]
            srid: u32,
        }
        fn default_srid() -> u32 {
            SRID_WGS84
        }
        let r = Raw::deserialize(deserializer)?;
        Ok(Self {
            x: r.x,
            y: r.y,
            srid: r.srid,
        })
    }
}

// ---- PostGIS EWKB wire format ---------------------------------------
//
// A 2-D Point with SRID is 25 bytes, little-endian:
//   [u8 byte-order = 0x01]
//   [u32 type      = 0x20000001]  (Point type 0x01 | SRID flag 0x20000000)
//   [u32 srid]
//   [f64 x]
//   [f64 y]
// Verified against `ST_AsEWKB(ST_SetSRID(ST_MakePoint(x,y), srid))`.

/// PostGIS geometry type code for a 2-D Point.
#[cfg(feature = "postgres")]
const WKB_POINT: u32 = 0x0000_0001;
/// EWKB flag bit indicating an embedded SRID follows the type word.
#[cfg(feature = "postgres")]
const EWKB_SRID_FLAG: u32 = 0x2000_0000;

#[cfg(feature = "postgres")]
fn encode_ewkb_point(p: &Point, buf: &mut Vec<u8>) {
    buf.push(0x01); // little-endian
    buf.extend_from_slice(&(WKB_POINT | EWKB_SRID_FLAG).to_le_bytes());
    buf.extend_from_slice(&p.srid.to_le_bytes());
    buf.extend_from_slice(&p.x.to_le_bytes());
    buf.extend_from_slice(&p.y.to_le_bytes());
}

#[cfg(feature = "postgres")]
fn decode_ewkb_point(bytes: &[u8]) -> Result<Point, sqlx::error::BoxDynError> {
    if bytes.len() < 5 {
        return Err("EWKB geometry too short (missing byte-order + type)".into());
    }
    let little = match bytes[0] {
        0x01 => true,
        0x00 => false,
        other => return Err(format!("EWKB: unknown byte-order marker {other:#x}").into()),
    };
    let rd_u32 = |b: &[u8]| -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };
    let rd_f64 = |b: &[u8]| -> f64 {
        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        if little {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        }
    };

    let type_word = rd_u32(&bytes[1..5]);
    if type_word & 0x0000_00ff != WKB_POINT {
        return Err(format!(
            "EWKB: expected a Point geometry, got type word {type_word:#x} (only Point is supported, issue #443)"
        )
        .into());
    }
    let has_srid = type_word & EWKB_SRID_FLAG != 0;
    let mut off = 5;
    let srid = if has_srid {
        if bytes.len() < off + 4 {
            return Err("EWKB: SRID flag set but value truncated".into());
        }
        let s = rd_u32(&bytes[off..off + 4]);
        off += 4;
        s
    } else {
        SRID_WGS84
    };
    if bytes.len() < off + 16 {
        return Err("EWKB Point: coordinate payload truncated".into());
    }
    let x = rd_f64(&bytes[off..off + 8]);
    let y = rd_f64(&bytes[off + 8..off + 16]);
    Ok(Point { x, y, srid })
}

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for Point {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        // PostGIS's `geometry` is an extension type with a dynamic OID;
        // resolve it by name.
        sqlx::postgres::PgTypeInfo::with_name("geometry")
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        use sqlx::TypeInfo as _;
        ty.name().eq_ignore_ascii_case("geometry")
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Encode<'_, sqlx::Postgres> for Point {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        let mut bytes = Vec::with_capacity(25);
        encode_ewkb_point(self, &mut bytes);
        buf.extend_from_slice(&bytes);
        Ok(sqlx::encode::IsNull::No)
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Decode<'_, sqlx::Postgres> for Point {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        // PostGIS returns geometry in binary as EWKB. In text mode it's
        // the hex-encoded EWKB (e.g. `0101000020E6100000…`).
        match value.format() {
            sqlx::postgres::PgValueFormat::Binary => decode_ewkb_point(value.as_bytes()?),
            sqlx::postgres::PgValueFormat::Text => {
                let hex = value.as_str()?.trim();
                let raw = decode_hex(hex)?;
                decode_ewkb_point(&raw)
            }
        }
    }
}

#[cfg(feature = "postgres")]
fn decode_hex(s: &str) -> Result<Vec<u8>, sqlx::error::BoxDynError> {
    if s.len() % 2 != 0 {
        return Err("EWKB hex: odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

#[cfg(feature = "mysql")]
impl sqlx::Type<sqlx::MySql> for Point {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Decode<'_, sqlx::MySql> for Point {
    fn decode(_value: sqlx::mysql::MySqlValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        Err(
            "`Point` columns are PostgreSQL/PostGIS-only; cannot decode on MySQL (issue #443)"
                .into(),
        )
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for Point {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Decode<'_, sqlx::Sqlite> for Point {
    fn decode(_value: sqlx::sqlite::SqliteValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        Err(
            "`Point` columns are PostgreSQL/PostGIS-only; cannot decode on SQLite (issue #443)"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_to_wgs84_and_wkt() {
        let p = Point::new(1.5, -2.25);
        assert_eq!(p.srid, SRID_WGS84);
        assert_eq!(p.to_wkt(), "POINT(1.5 -2.25)");
    }

    #[test]
    fn into_sqlvalue_geometry() {
        let sv: crate::core::SqlValue = Point::with_srid(1.0, 2.0, 3857).into();
        match sv {
            crate::core::SqlValue::Geometry { x, y, srid } => {
                assert_eq!((x, y, srid), (1.0, 2.0, 3857));
            }
            _ => panic!("expected SqlValue::Geometry"),
        }
    }

    #[test]
    fn serde_round_trips_as_object() {
        let p = Point::with_srid(1.5, -2.25, 4326);
        let json = serde_json::to_string(&p).unwrap();
        let back: Point = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        // srid defaults when omitted.
        let p2: Point = serde_json::from_str(r#"{"x":1.0,"y":2.0}"#).unwrap();
        assert_eq!(p2, Point::new(1.0, 2.0));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn ewkb_round_trip_and_matches_postgis() {
        let p = Point::with_srid(1.5, -2.25, 4326);
        let mut buf = Vec::new();
        encode_ewkb_point(&p, &mut buf);
        // Byte-for-byte equal to PostGIS `ST_AsEWKB(ST_SetSRID(ST_MakePoint(1.5,-2.25),4326))`:
        // 0101000020e6100000000000000000f83f00000000000002c0
        let expected = decode_hex("0101000020e6100000000000000000f83f00000000000002c0").unwrap();
        assert_eq!(buf, expected, "EWKB encoding must match PostGIS");
        assert_eq!(decode_ewkb_point(&buf).unwrap(), p);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn decode_rejects_non_point() {
        // LineString (type 0x02) with SRID flag → rejected.
        let mut bytes = vec![0x01];
        bytes.extend_from_slice(&(0x0000_0002u32 | EWKB_SRID_FLAG).to_le_bytes());
        bytes.extend_from_slice(&4326u32.to_le_bytes());
        assert!(decode_ewkb_point(&bytes).is_err());
    }
}
