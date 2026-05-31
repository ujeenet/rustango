//! Issue #524 — `#[derive(Model)]` accepts `Decimal` / `NaiveTime` /
//! `Vec<u8>` and maps each to the right `FieldType` (`Decimal`,
//! `Time`, `Binary`). Before this fix, the macro rejected these
//! types outright in `detect_type` (`rustango-macros/src/lib.rs`)
//! and showcase apps had to fall back to `i64` for money columns.
//!
//! The test is a pure macro-emission check — never touches a DB.
//! Build-only is enough to prove the macro accepts the types and
//! emits the right `FieldType`, without entangling the test with
//! sqlx's per-backend `Decode/Type` impls.
//!
//! ## Backend split
//!
//! `chrono::NaiveTime` and `Vec<u8>` have `Decode`/`Type` impls for
//! every sqlx backend, so the `WaresTimeBin` struct compiles
//! anywhere.
//!
//! `rust_decimal::Decimal` only has impls for `Postgres` + `MySql`
//! in sqlx 0.8 (sqlite has no native NUMERIC type and ships no
//! `Decimal: Decode<Sqlite>` impl — documented in
//! `sql::executor::mod.rs::bind_match_sqlite!`). The macro's
//! `__impl_sqlite_from_row!` fires unconditionally when the sqlite
//! feature is on, so the `WaresDecimal` struct has to be gated to
//! `not(feature = "sqlite")` builds.

#![allow(dead_code)]

use rustango::core::FieldType;
use rustango::core::Model as _;
use rustango::core::SqlValue;
use rustango::sql::Auto;
use rustango::Model;

// ---- Universal: Time + Binary work on every backend. ---------------

#[derive(Model, Debug, Clone)]
#[rustango(table = "wares_524_timebin")]
pub struct WaresTimeBin {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub opens: chrono::NaiveTime,
    pub payload: Vec<u8>,
}

#[test]
fn time_and_binary_emit_right_field_types() {
    let fields: Vec<(&'static str, FieldType)> = WaresTimeBin::SCHEMA
        .scalar_fields()
        .map(|f| (f.name, f.ty))
        .collect();
    assert!(
        fields.contains(&("opens", FieldType::Time)),
        "NaiveTime → FieldType::Time: {fields:?}"
    );
    assert!(
        fields.contains(&("payload", FieldType::Binary)),
        "Vec<u8> → FieldType::Binary: {fields:?}"
    );
}

#[test]
fn sqlvalue_into_for_time_and_binary() {
    // Smoke that the macro's emitted `Into<SqlValue>` walks each
    // type cleanly — without these, the `_columns/_values` push
    // chain in the generated `insert_pool` would fail to compile.
    let t: SqlValue = chrono::NaiveTime::from_hms_opt(9, 30, 0).unwrap().into();
    assert!(matches!(t, SqlValue::Time(_)));

    let b: SqlValue = vec![0u8, 1, 2, 3].into();
    assert!(matches!(b, SqlValue::Binary(_)));
}

// ---- Decimal: PG + MySQL only (sqlx-sqlite has no impl). -----------

#[cfg(all(any(feature = "postgres", feature = "mysql"), not(feature = "sqlite")))]
mod decimal_pg_mysql {
    use super::*;
    use rust_decimal::Decimal;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "wares_524_decimal")]
    pub struct WaresDecimal {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        pub price: Decimal,
    }

    #[test]
    fn decimal_emits_right_field_type() {
        let fields: Vec<(&'static str, FieldType)> = WaresDecimal::SCHEMA
            .scalar_fields()
            .map(|f| (f.name, f.ty))
            .collect();
        assert!(
            fields.contains(&("price", FieldType::Decimal)),
            "Decimal → FieldType::Decimal: {fields:?}"
        );
    }

    #[test]
    fn sqlvalue_into_for_decimal() {
        let d: SqlValue = Decimal::new(1995, 2).into();
        assert!(matches!(d, SqlValue::Decimal(_)));
    }
}
