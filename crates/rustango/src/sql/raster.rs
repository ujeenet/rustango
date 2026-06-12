//! `Raster` — PostGIS `raster` column wrapper (GeoDjango `gis.gdal`
//! raster support, issue #444).
//!
//! Declare a `raster` column on a model and round-trip it as a Rust
//! `Raster`:
//!
//! ```ignore
//! #[derive(Model)]
//! #[rustango(table = "tile")]
//! struct Tile {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,
//!     // → DDL `coverage raster`; round-trips as a `Raster`. The `Raster`
//!     // Rust type alone drives detection — no field attribute needed.
//!     coverage: Raster,
//! }
//! ```
//!
//! ## What this covers (and what it doesn't)
//!
//! `Raster` round-trips the PostGIS `raster` value **losslessly** (it
//! holds the raw WKB-raster bytes) and decodes the fixed 61-byte header
//! so you can read the georeference + dimensions ([`Self::width`] /
//! [`Self::height`] / [`Self::srid`] / [`Self::num_bands`] /
//! [`Self::scale_x`] …). Pixel-band *data* is preserved verbatim but not
//! parsed in this slice — drive band math with PostGIS `ST_*` raster
//! functions in raw SQL. (GDAL file ingest / per-pixel access is a
//! follow-up; this is the storage + georeference type, the raster analog
//! of [`crate::sql::Point`] / #443.)
//!
//! ## PostgreSQL/PostGIS only, by language semantics
//!
//! `raster` is a PostGIS extension type (the `postgis_raster` extension
//! since PostGIS 3.0). Like `geometry` / `vector`, `Raster` is **PG-only
//! by language semantics**: the migration writer emits a degraded `TEXT`
//! column on MySQL / SQLite and the [`sqlx::Decode`] path errors there.
//! The type still *compiles* under every backend (the per-backend
//! [`sqlx::Type`] / [`sqlx::Decode`] impls below are total).

/// The fixed WKB-raster header size, in bytes: byte-order (1) + version
/// (2) + nBands (2) + 6×f64 georeference (48) + srid (4) + width (2) +
/// height (2). Band data, if any, follows.
const HEADER_LEN: usize = 61;

/// A PostGIS `raster` value — see the [module docs](self). A transparent
/// newtype over the raw WKB-raster bytes, so it round-trips losslessly;
/// the header accessors decode the georeference on demand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Raster(pub Vec<u8>);

impl Raster {
    /// Wrap raw WKB-raster bytes (e.g. from `ST_AsBinary(rast)`).
    #[must_use]
    pub fn from_wkb(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The raw WKB-raster bytes.
    #[must_use]
    pub fn as_wkb(&self) -> &[u8] {
        &self.0
    }

    /// Consume, returning the raw WKB-raster bytes.
    #[must_use]
    pub fn into_wkb(self) -> Vec<u8> {
        self.0
    }

    /// Lowercase-hex of the WKB bytes — PostGIS's text input form for
    /// `raster` (`'<hex>'::raster`). The bind path uses this because the
    /// `raster` type has no *binary* input function (only output), so
    /// inserts go in as hex text + a `::raster` cast.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(self.0.len() * 2);
        for byte in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{byte:02x}");
        }
        s
    }

    /// `true` if the buffer is at least a full WKB-raster header.
    #[must_use]
    pub fn has_header(&self) -> bool {
        self.0.len() >= HEADER_LEN && matches!(self.0[0], 0x00 | 0x01)
    }

    fn little(&self) -> bool {
        self.0.first() != Some(&0x00)
    }

    fn u16_at(&self, off: usize) -> u16 {
        let a = [self.0[off], self.0[off + 1]];
        if self.little() {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }

    fn i32_at(&self, off: usize) -> i32 {
        let a = [
            self.0[off],
            self.0[off + 1],
            self.0[off + 2],
            self.0[off + 3],
        ];
        if self.little() {
            i32::from_le_bytes(a)
        } else {
            i32::from_be_bytes(a)
        }
    }

    fn f64_at(&self, off: usize) -> f64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&self.0[off..off + 8]);
        if self.little() {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        }
    }

    /// WKB-raster format version (header bytes 1..3). `None` if the
    /// buffer is too short to hold a header.
    #[must_use]
    pub fn version(&self) -> Option<u16> {
        self.has_header().then(|| self.u16_at(1))
    }

    /// Number of pixel bands (header bytes 3..5).
    #[must_use]
    pub fn num_bands(&self) -> Option<u16> {
        self.has_header().then(|| self.u16_at(3))
    }

    /// Pixel width X scale (`scaleX`).
    #[must_use]
    pub fn scale_x(&self) -> Option<f64> {
        self.has_header().then(|| self.f64_at(5))
    }

    /// Pixel height Y scale (`scaleY`, typically negative — north-up).
    #[must_use]
    pub fn scale_y(&self) -> Option<f64> {
        self.has_header().then(|| self.f64_at(13))
    }

    /// Upper-left corner X (`ipX`) in the raster's SRID.
    #[must_use]
    pub fn upper_left_x(&self) -> Option<f64> {
        self.has_header().then(|| self.f64_at(21))
    }

    /// Upper-left corner Y (`ipY`).
    #[must_use]
    pub fn upper_left_y(&self) -> Option<f64> {
        self.has_header().then(|| self.f64_at(29))
    }

    /// Row rotation about the X axis (`skewX`, usually 0).
    #[must_use]
    pub fn skew_x(&self) -> Option<f64> {
        self.has_header().then(|| self.f64_at(37))
    }

    /// Column rotation about the Y axis (`skewY`, usually 0).
    #[must_use]
    pub fn skew_y(&self) -> Option<f64> {
        self.has_header().then(|| self.f64_at(45))
    }

    /// Spatial reference identifier (header bytes 53..57).
    #[must_use]
    pub fn srid(&self) -> Option<i32> {
        self.has_header().then(|| self.i32_at(53))
    }

    /// Raster width in pixels (header bytes 57..59).
    #[must_use]
    pub fn width(&self) -> Option<u16> {
        self.has_header().then(|| self.u16_at(57))
    }

    /// Raster height in pixels (header bytes 59..61).
    #[must_use]
    pub fn height(&self) -> Option<u16> {
        self.has_header().then(|| self.u16_at(59))
    }

    /// Build a band-less raster (a valid PostGIS `raster` with a header
    /// and zero bands) — the round-trip-test / placeholder constructor,
    /// the analog of `ST_MakeEmptyRaster(width, height, ipx, ipy,
    /// scalex, scaley, skewx, skewy, srid)`. Emits little-endian WKB.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn empty(
        width: u16,
        height: u16,
        upper_left_x: f64,
        upper_left_y: f64,
        scale_x: f64,
        scale_y: f64,
        skew_x: f64,
        skew_y: f64,
        srid: i32,
    ) -> Self {
        let mut b = Vec::with_capacity(HEADER_LEN);
        b.push(0x01); // little-endian
        b.extend_from_slice(&0u16.to_le_bytes()); // version
        b.extend_from_slice(&0u16.to_le_bytes()); // nBands
        b.extend_from_slice(&scale_x.to_le_bytes());
        b.extend_from_slice(&scale_y.to_le_bytes());
        b.extend_from_slice(&upper_left_x.to_le_bytes());
        b.extend_from_slice(&upper_left_y.to_le_bytes());
        b.extend_from_slice(&skew_x.to_le_bytes());
        b.extend_from_slice(&skew_y.to_le_bytes());
        b.extend_from_slice(&srid.to_le_bytes());
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        Self(b)
    }
}

// ---- `Raster` → `SqlValue` (INSERT / UPDATE bind, via SqlValue::Raster) ----

impl From<Raster> for crate::core::SqlValue {
    fn from(r: Raster) -> Self {
        crate::core::SqlValue::Raster(r.0)
    }
}

// ---- serde: lossless lowercase-hex string of the WKB bytes ----------

impl serde::Serialize for Raster {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for Raster {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("raster hex: odd length"));
        }
        let bytes = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(serde::de::Error::custom)?;
        Ok(Self(bytes))
    }
}

// ---- PostGIS `raster` wire format = WKB-raster bytes ----------------

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for Raster {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        // PostGIS's `raster` is an extension type with a dynamic OID;
        // resolve it by name.
        sqlx::postgres::PgTypeInfo::with_name("raster")
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        use sqlx::TypeInfo as _;
        ty.name().eq_ignore_ascii_case("raster")
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Encode<'_, sqlx::Postgres> for Raster {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        buf.extend_from_slice(&self.0);
        Ok(sqlx::encode::IsNull::No)
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Decode<'_, sqlx::Postgres> for Raster {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        // PostGIS returns `raster` in binary as the WKB-raster bytes; in
        // text mode it's the hex-encoded WKB.
        match value.format() {
            sqlx::postgres::PgValueFormat::Binary => Ok(Self(value.as_bytes()?.to_vec())),
            sqlx::postgres::PgValueFormat::Text => {
                let hex = value.as_str()?.trim();
                if hex.len() % 2 != 0 {
                    return Err("raster hex: odd length".into());
                }
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(Into::into))
                    .collect::<Result<Vec<u8>, sqlx::error::BoxDynError>>()?;
                Ok(Self(bytes))
            }
        }
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Type<sqlx::MySql> for Raster {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Decode<'_, sqlx::MySql> for Raster {
    fn decode(_value: sqlx::mysql::MySqlValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        Err(
            "`Raster` columns are PostgreSQL/PostGIS-only; cannot decode on MySQL (issue #444)"
                .into(),
        )
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for Raster {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Decode<'_, sqlx::Sqlite> for Raster {
    fn decode(_value: sqlx::sqlite::SqliteValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        Err(
            "`Raster` columns are PostgreSQL/PostGIS-only; cannot decode on SQLite (issue #444)"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_header_round_trips_accessors() {
        // Mirrors `ST_MakeEmptyRaster(2, 3, 10, 20, 1, -1, 0, 0, 4326)`.
        let r = Raster::empty(2, 3, 10.0, 20.0, 1.0, -1.0, 0.0, 0.0, 4326);
        assert_eq!(r.width(), Some(2));
        assert_eq!(r.height(), Some(3));
        assert_eq!(r.srid(), Some(4326));
        assert_eq!(r.num_bands(), Some(0));
        assert_eq!(r.scale_x(), Some(1.0));
        assert_eq!(r.scale_y(), Some(-1.0));
        assert_eq!(r.upper_left_x(), Some(10.0));
        assert_eq!(r.upper_left_y(), Some(20.0));
        assert_eq!(r.version(), Some(0));
    }

    #[test]
    fn empty_matches_postgis_wkb() {
        // Byte-for-byte equal to PostGIS
        // `ST_AsBinary(ST_MakeEmptyRaster(2,3,10,20,1,-1,0,0,4326))`.
        let r = Raster::empty(2, 3, 10.0, 20.0, 1.0, -1.0, 0.0, 0.0, 4326);
        let expected = "0100000000000000000000f03f000000000000f0bf0000000000002440000000000000344000000000000000000000000000000000e610000002000300";
        let hex: String = r.0.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected, "empty raster WKB must match PostGIS");
    }

    #[test]
    fn short_buffer_has_no_header() {
        let r = Raster::from_wkb(vec![0x01, 0x00]);
        assert!(!r.has_header());
        assert_eq!(r.width(), None);
    }

    #[test]
    fn serde_round_trips_as_hex() {
        let r = Raster::empty(4, 5, 0.0, 0.0, 1.0, -1.0, 0.0, 0.0, 4326);
        let json = serde_json::to_string(&r).unwrap();
        let back: Raster = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn into_sqlvalue_raster() {
        let sv: crate::core::SqlValue = Raster::from_wkb(vec![1, 2, 3]).into();
        match sv {
            crate::core::SqlValue::Raster(b) => assert_eq!(b, vec![1, 2, 3]),
            _ => panic!("expected SqlValue::Raster"),
        }
    }
}
