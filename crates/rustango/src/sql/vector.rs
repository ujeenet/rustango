//! `Vector` — pgvector embedding column wrapper (Eloquent 13
//! `vector(...)` / similarity search, issue #824).
//!
//! Declare a `vector(N)` column on a model and round-trip it as a Rust
//! `Vec<f32>`:
//!
//! ```ignore
//! #[derive(Model)]
//! #[rustango(table = "doc")]
//! struct Doc {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,
//!     // → DDL `embedding vector(3)`; round-trips as a `Vec<f32>`.
//!     #[rustango(vector(dims = 3))]
//!     embedding: Vector,
//! }
//! ```
//!
//! Pairs with the distance operators
//! ([`crate::core::BinOp::L2Distance`] / `CosineDistance` /
//! `InnerProduct`, pgvector `<->` / `<=>` / `<#>`) and the queryset
//! helpers [`crate::query::QuerySet::order_by_distance`] /
//! [`k_nearest`](crate::query::QuerySet::k_nearest).
//!
//! ## PostgreSQL only, by language semantics
//!
//! `vector` is a Postgres extension type (pgvector). Like trigram /
//! full-text / `Array<T>`, `Vector` is **PG-only by language
//! semantics**: the migration writer emits a degraded `TEXT` column on
//! MySQL / SQLite, and the [`sqlx::Decode`] path + the distance
//! operators raise a clear error on those backends rather than silently
//! mis-storing. The type still *compiles* under every backend (the
//! per-backend [`sqlx::Type`] / [`sqlx::Decode`] impls below are total),
//! so the `sqlite,tenancy` litmus build keeps passing.
//!
//! ## Why a newtype rather than a bare `Vec<f32>`
//!
//! Same rationale as [`crate::sql::Array`]: `#[derive(Model)]` emits one
//! shared `FromRow` body reused across the PG / MySQL / SQLite decoders.
//! A bare `Vec<f32>` doesn't decode the pgvector `vector` type at all,
//! and would fail to compile the non-PG decoder arms. `Vector` carries
//! total `Decode` / `Type` impls for all three backends (a real
//! pgvector binary codec on PG, erroring stubs elsewhere).

use std::ops::{Deref, DerefMut};

/// pgvector embedding column — see the [module docs](self). Transparent
/// newtype over `Vec<f32>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Vector(pub Vec<f32>);

impl Vector {
    /// Wrap a `Vec<f32>` as a vector column value.
    #[must_use]
    pub fn new(items: Vec<f32>) -> Self {
        Self(items)
    }

    /// Consume the wrapper, returning the inner `Vec<f32>`.
    #[must_use]
    pub fn into_inner(self) -> Vec<f32> {
        self.0
    }

    /// pgvector text representation — `[1,2,3]`. Used when binding the
    /// query vector as a text literal isn't needed (we bind the binary
    /// form), but handy for diagnostics + the text decode fallback.
    #[must_use]
    pub fn to_pg_text(&self) -> String {
        let mut s = String::with_capacity(self.0.len() * 4 + 2);
        s.push('[');
        for (i, v) in self.0.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&v.to_string());
        }
        s.push(']');
        s
    }
}

impl Deref for Vector {
    type Target = Vec<f32>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Vector {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<f32>> for Vector {
    fn from(v: Vec<f32>) -> Self {
        Self(v)
    }
}

impl From<Vector> for Vec<f32> {
    fn from(v: Vector) -> Self {
        v.0
    }
}

impl FromIterator<f32> for Vector {
    fn from_iter<I: IntoIterator<Item = f32>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

// ---- serde: behave exactly like the inner Vec (plain JSON array) ----

impl serde::Serialize for Vector {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Vector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<f32>::deserialize(deserializer).map(Self)
    }
}

// ---- `Vector` → `SqlValue` (INSERT / UPDATE bind, via SqlValue::Vector) ----

impl From<Vector> for crate::core::SqlValue {
    fn from(v: Vector) -> Self {
        crate::core::SqlValue::Vector(v.0)
    }
}

// ---- pgvector binary wire format -------------------------------------
//
// A `vector` value is `[int16 dim][int16 unused][float4 × dim]`, all
// big-endian. (Matches the `pgvector` crate's encoding so we don't take
// a dependency on it.)

#[cfg(feature = "postgres")]
fn encode_pgvector(v: &[f32], buf: &mut Vec<u8>) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let dim = v.len() as i16;
    buf.extend_from_slice(&dim.to_be_bytes());
    buf.extend_from_slice(&0i16.to_be_bytes()); // unused
    for &x in v {
        buf.extend_from_slice(&x.to_be_bytes());
    }
}

#[cfg(feature = "postgres")]
fn decode_pgvector_binary(bytes: &[u8]) -> Result<Vec<f32>, sqlx::error::BoxDynError> {
    if bytes.len() < 4 {
        return Err("pgvector binary value too short (missing header)".into());
    }
    let dim = i16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let body = &bytes[4..];
    if body.len() != dim * 4 {
        return Err(format!(
            "pgvector binary value: header dim {dim} != {} float4 payload bytes",
            body.len()
        )
        .into());
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in body.chunks_exact(4) {
        out.push(f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
fn decode_pgvector_text(s: &str) -> Result<Vec<f32>, sqlx::error::BoxDynError> {
    // `[1,2,3]`
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|p| p.trim().parse::<f32>().map_err(Into::into))
        .collect()
}

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for Vector {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        // pgvector's `vector` is an extension type with a dynamic OID;
        // resolve it by name.
        sqlx::postgres::PgTypeInfo::with_name("vector")
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        use sqlx::TypeInfo as _;
        ty.name().eq_ignore_ascii_case("vector")
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Encode<'_, sqlx::Postgres> for Vector {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        let mut bytes = Vec::with_capacity(4 + self.0.len() * 4);
        encode_pgvector(&self.0, &mut bytes);
        buf.extend_from_slice(&bytes);
        Ok(sqlx::encode::IsNull::No)
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Decode<'_, sqlx::Postgres> for Vector {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        let inner = match value.format() {
            sqlx::postgres::PgValueFormat::Binary => decode_pgvector_binary(value.as_bytes()?)?,
            sqlx::postgres::PgValueFormat::Text => decode_pgvector_text(value.as_str()?)?,
        };
        Ok(Self(inner))
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Type<sqlx::MySql> for Vector {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Decode<'_, sqlx::MySql> for Vector {
    fn decode(_value: sqlx::mysql::MySqlValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        Err(
            "`Vector` columns are PostgreSQL/pgvector-only; cannot decode on MySQL (issue #824)"
                .into(),
        )
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for Vector {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Decode<'_, sqlx::Sqlite> for Vector {
    fn decode(_value: sqlx::sqlite::SqliteValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        Err(
            "`Vector` columns are PostgreSQL/pgvector-only; cannot decode on SQLite (issue #824)"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deref_from_vec_and_text() {
        let v: Vector = vec![1.0, 2.0, 3.0].into();
        assert_eq!(v.len(), 3);
        assert_eq!(v.to_pg_text(), "[1,2,3]");
        assert_eq!(v.into_inner(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn serde_round_trips_as_plain_array() {
        let v = Vector::new(vec![0.5, 1.5]);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "[0.5,1.5]");
        let back: Vector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn into_sqlvalue_vector() {
        let sv: crate::core::SqlValue = Vector(vec![1.0, 2.0]).into();
        match sv {
            crate::core::SqlValue::Vector(items) => assert_eq!(items, vec![1.0, 2.0]),
            _ => panic!("expected SqlValue::Vector"),
        }
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pgvector_binary_round_trip() {
        let mut buf = Vec::new();
        encode_pgvector(&[1.0, -2.5, 3.0], &mut buf);
        assert_eq!(decode_pgvector_binary(&buf).unwrap(), vec![1.0, -2.5, 3.0]);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pgvector_text_parse() {
        assert_eq!(
            decode_pgvector_text("[1,2,3]").unwrap(),
            vec![1.0, 2.0, 3.0]
        );
        assert_eq!(decode_pgvector_text("[]").unwrap(), Vec::<f32>::new());
    }
}
