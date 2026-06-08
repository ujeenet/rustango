//! `Range<T>` — typed PostgreSQL range column wrapper (Django's
//! `RangeField` family, issue #343).
//!
//! Declare a native PG range column on a model:
//!
//! ```ignore
//! use rustango::sql::{Auto, Range};
//! use std::ops::Bound;
//!
//! #[derive(Model)]
//! #[rustango(table = "event")]
//! struct Event {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,
//!     // → DDL `during tstzrange`; round-trips as a Rust `Range<DateTime<Utc>>`.
//!     during: Range<chrono::DateTime<chrono::Utc>>,
//!     // → DDL `seats int4range`.
//!     seats: Range<i32>,
//! }
//!
//! let e = Event {
//!     id: Auto::default(),
//!     during: Range::closed_open(start, end),  // [start, end)
//!     seats: (1..100).into(),                   // [1, 100)
//! };
//! ```
//!
//! The element type drives the column type emitted by the migration
//! writer:
//!
//! | Rust field                       | PG column   | Django field            |
//! |----------------------------------|-------------|-------------------------|
//! | `Range<i32>`                     | `int4range` | `IntegerRangeField`     |
//! | `Range<i64>`                     | `int8range` | `BigIntegerRangeField`  |
//! | `Range<rust_decimal::Decimal>`   | `numrange`  | `DecimalRangeField`     |
//! | `Range<chrono::NaiveDate>`       | `daterange` | `DateRangeField`        |
//! | `Range<chrono::DateTime<Utc>>`   | `tstzrange` | `DateTimeRangeField`    |
//!
//! Pairs with the already-shipped range operators
//! ([`crate::core::Op::RangeContains`] / `RangeContainedBy` /
//! `RangeOverlap` / `RangeStrictlyLeft` / `RangeStrictlyRight` /
//! `RangeAdjacent`, PG `@>` / `<@` / `&&` / `<<` / `>>` / `-|-`).
//!
//! ## PostgreSQL only, by language semantics
//!
//! Range types are a Postgres feature; MySQL and SQLite have no
//! equivalent. Like [`Array`](crate::sql::Array) / trigram / full-text,
//! `Range<T>` is **PG-only by language semantics**: the migration writer
//! emits a degraded `TEXT` column on MySQL / SQLite and the decode path
//! errors there. The type still *compiles* under every backend (the
//! per-backend [`sqlx::Type`] / [`sqlx::Decode`] impls are total) so the
//! `sqlite,tenancy` litmus build keeps passing.
//!
//! ## Binding & decoding
//!
//! On INSERT/UPDATE a `Range<T>` lowers to a PostgreSQL range *literal*
//! string (`"[1,10)"`, `"[1,)"`, `"(,10)"`) via
//! [`crate::core::SqlValue::RangeLiteral`], which PG implicit-casts to
//! the column's range type. On SELECT it decodes through sqlx's
//! `PgRange<T>`. Postgres normalizes discrete ranges (int / date) to the
//! canonical `[lower, upper)` form on storage, so a stored `[1,10]`
//! reads back as `[1,11)`.

use std::ops::Bound;

/// Typed PostgreSQL range column — see the [module docs](self).
///
/// A lower + upper [`Bound`]. Build via [`Self::closed_open`] /
/// [`Self::new`] / `From<std::ops::Range<T>>` (`a..b` → `[a, b)`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Range<T> {
    /// Lower bound. `Included` → `[`, `Excluded` → `(`, `Unbounded` → no
    /// lower limit.
    pub lower: Bound<T>,
    /// Upper bound. `Included` → `]`, `Excluded` → `)`, `Unbounded` → no
    /// upper limit.
    pub upper: Bound<T>,
}

impl<T> Range<T> {
    /// Construct from explicit bounds.
    #[must_use]
    pub fn new(lower: Bound<T>, upper: Bound<T>) -> Self {
        Self { lower, upper }
    }

    /// `[lower, upper)` — the canonical half-open range (lower inclusive,
    /// upper exclusive). Matches PostgreSQL's normalized discrete-range
    /// form and Python's `range`/Django default.
    #[must_use]
    pub fn closed_open(lower: T, upper: T) -> Self {
        Self {
            lower: Bound::Included(lower),
            upper: Bound::Excluded(upper),
        }
    }

    /// `[lower, ∞)` — lower-bounded, no upper limit.
    #[must_use]
    pub fn at_least(lower: T) -> Self {
        Self {
            lower: Bound::Included(lower),
            upper: Bound::Unbounded,
        }
    }

    /// `(∞, upper)` — upper-bounded, no lower limit.
    #[must_use]
    pub fn less_than(upper: T) -> Self {
        Self {
            lower: Bound::Unbounded,
            upper: Bound::Excluded(upper),
        }
    }
}

impl<T> From<std::ops::Range<T>> for Range<T> {
    /// `a..b` → `[a, b)`.
    fn from(r: std::ops::Range<T>) -> Self {
        Self::closed_open(r.start, r.end)
    }
}

impl<T> From<(Bound<T>, Bound<T>)> for Range<T> {
    fn from(v: (Bound<T>, Bound<T>)) -> Self {
        Self {
            lower: v.0,
            upper: v.1,
        }
    }
}

/// Render a PostgreSQL range literal — `[lo,hi)` with bracket chosen per
/// bound inclusivity; an unbounded side emits an empty value. `fmt`
/// formats one endpoint value the way PG's range parser accepts it.
fn pg_range_literal<T>(lower: &Bound<T>, upper: &Bound<T>, fmt: impl Fn(&T) -> String) -> String {
    let (open, lo) = match lower {
        Bound::Included(v) => ('[', fmt(v)),
        Bound::Excluded(v) => ('(', fmt(v)),
        Bound::Unbounded => ('[', String::new()),
    };
    let (hi, close) = match upper {
        Bound::Included(v) => (fmt(v), ']'),
        Bound::Excluded(v) => (fmt(v), ')'),
        Bound::Unbounded => (String::new(), ')'),
    };
    format!("{open}{lo},{hi}{close}")
}

// ---- serde: a `{ "lower": ..., "upper": ... }` object with bound tags ----
//
// Derives delegate to `Bound<T>`'s serde (an enum). Good enough for
// DRF/JSON round-trips and inspection; not the PG literal form.

impl<T: serde::Serialize> serde::Serialize for Range<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Range", 2)?;
        s.serialize_field("lower", &BoundSer(&self.lower))?;
        s.serialize_field("upper", &BoundSer(&self.upper))?;
        s.end()
    }
}

/// Serialize a `Bound<T>` as `null` (Unbounded) or the bare value;
/// inclusivity is implied by convention (`[lower, upper)`).
struct BoundSer<'a, T>(&'a Bound<T>);

impl<T: serde::Serialize> serde::Serialize for BoundSer<'_, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Bound::Included(v) | Bound::Excluded(v) => v.serialize(serializer),
            Bound::Unbounded => serializer.serialize_none(),
        }
    }
}

// ---- `Range<T>` → `SqlValue::RangeLiteral` (INSERT / UPDATE bind) ----
//
// PG implicit-casts the literal string to the column's range type. One
// concrete impl per element type so the endpoint formatting matches what
// PG's range parser accepts (ISO date/timestamp, plain numerics).

macro_rules! range_into_sqlvalue {
    ($t:ty, $fmt:expr) => {
        impl From<Range<$t>> for crate::core::SqlValue {
            fn from(r: Range<$t>) -> Self {
                let f: fn(&$t) -> String = $fmt;
                crate::core::SqlValue::RangeLiteral(pg_range_literal(&r.lower, &r.upper, f))
            }
        }
    };
}

range_into_sqlvalue!(i32, |v| v.to_string());
range_into_sqlvalue!(i64, |v| v.to_string());
range_into_sqlvalue!(rust_decimal::Decimal, |v| v.to_string());
range_into_sqlvalue!(chrono::NaiveDate, |v| v.to_string());
range_into_sqlvalue!(chrono::DateTime<chrono::Utc>, |v| v.to_rfc3339());

// ---- sqlx Type + Decode (FromRow read path) ----
//
// Postgres: delegate to sqlx's `PgRange<T>` (the real range decode), then
// copy its `start`/`end` bounds. MySQL / SQLite: total stubs so the
// shared `FromRow` body compiles — `Type` borrows a placeholder
// type-info, `Decode` errors at runtime. Encode is intentionally NOT
// implemented: the bind path goes through `SqlValue::RangeLiteral`.

#[cfg(feature = "postgres")]
impl<'r, T> sqlx::Decode<'r, sqlx::Postgres> for Range<T>
where
    sqlx::postgres::types::PgRange<T>: sqlx::Decode<'r, sqlx::Postgres>,
{
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        let pg =
            <sqlx::postgres::types::PgRange<T> as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
        Ok(Self {
            lower: pg.start,
            upper: pg.end,
        })
    }
}

#[cfg(feature = "postgres")]
impl<T> sqlx::Type<sqlx::Postgres> for Range<T>
where
    sqlx::postgres::types::PgRange<T>: sqlx::Type<sqlx::Postgres>,
{
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <sqlx::postgres::types::PgRange<T> as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <sqlx::postgres::types::PgRange<T> as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

#[cfg(feature = "mysql")]
impl<'r, T> sqlx::Decode<'r, sqlx::MySql> for Range<T> {
    fn decode(
        _value: <sqlx::MySql as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Err("`Range<T>` columns are PostgreSQL-only; cannot decode on MySQL (issue #343)".into())
    }
}

#[cfg(feature = "mysql")]
impl<T> sqlx::Type<sqlx::MySql> for Range<T> {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <String as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl<'r, T> sqlx::Decode<'r, sqlx::Sqlite> for Range<T> {
    fn decode(
        _value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        Err("`Range<T>` columns are PostgreSQL-only; cannot decode on SQLite (issue #343)".into())
    }
}

#[cfg(feature = "sqlite")]
impl<T> sqlx::Type<sqlx::Sqlite> for Range<T> {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <String as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SqlValue;

    #[test]
    fn closed_open_literal() {
        let v: SqlValue = Range::closed_open(1_i32, 10).into();
        assert!(matches!(v, SqlValue::RangeLiteral(ref s) if s == "[1,10)"));
    }

    #[test]
    fn unbounded_sides() {
        let lo: SqlValue = Range::at_least(5_i64).into();
        assert!(matches!(lo, SqlValue::RangeLiteral(ref s) if s == "[5,)"));
        let hi: SqlValue = Range::less_than(10_i64).into();
        assert!(matches!(hi, SqlValue::RangeLiteral(ref s) if s == "[,10)"));
    }

    #[test]
    fn from_std_range() {
        let r: Range<i32> = (1..10).into();
        assert_eq!(r.lower, Bound::Included(1));
        assert_eq!(r.upper, Bound::Excluded(10));
    }

    #[test]
    fn date_literal_is_iso() {
        let d1 = chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2025, 2, 1).unwrap();
        let v: SqlValue = Range::closed_open(d1, d2).into();
        assert!(matches!(v, SqlValue::RangeLiteral(ref s) if s == "[2025-01-01,2025-02-01)"));
    }

    #[test]
    fn serde_round_trips() {
        let r = Range::closed_open(1_i32, 10);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"lower":1,"upper":10}"#);
    }
}
