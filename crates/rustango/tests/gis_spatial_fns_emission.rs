//! Emission tests for PostGIS spatial functions (`ST_Distance`,
//! `ST_DWithin`, `ST_Contains`, `ST_Within`, `ST_Intersects`) — issue
//! #58, the query layer atop the #443 geometry type. PG emits the native
//! `ST_*` calls; MySQL and SQLite reject at compile time with
//! `OpNotSupportedInDialect`. No database needed — pure dialect compile.

use rustango::core::funcs::{st_contains, st_distance, st_dwithin, st_intersects, st_within};
use rustango::core::{
    Assignment, Expr, Filter, Model as _, Op, ScalarFn, SqlValue, UpdateQuery, WhereExpr, F,
};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Point, Postgres, SqlError};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "gis_emit_place")]
#[allow(dead_code)]
pub struct Place {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(geometry(srid = 4326))]
    location: Point,
    rank: f64,
}

// Park the spatial expr in a SET clause — we only inspect the emitted
// SQL / params, never execute, so type-compatibility is irrelevant.
fn update_set(value: Expr) -> UpdateQuery {
    UpdateQuery {
        model: Place::SCHEMA,
        set: vec![Assignment {
            column: "rank",
            value,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    }
}

// ---------- PG: native ST_* emission ----------

#[test]
fn pg_emits_st_distance() {
    let q = update_set(st_distance(F("location"), Point::new(1.0, 2.0)));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"ST_Distance("location", $"#),
        "PG ST_Distance: {}",
        stmt.sql
    );
    assert!(
        stmt.params
            .iter()
            .any(|p| matches!(p, SqlValue::Geometry { .. })),
        "point bound as a Geometry param: {:?}",
        stmt.params
    );
}

#[test]
fn pg_emits_st_dwithin() {
    let q = update_set(st_dwithin(
        F("location"),
        Point::new(1.0, 2.0),
        Expr::Literal(SqlValue::F64(0.5)),
    ));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"ST_DWithin("location", $"#),
        "PG ST_DWithin: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_topological_predicates() {
    for (build, label) in [
        (
            st_contains(F("location"), Point::new(0.0, 0.0)),
            "ST_Contains(",
        ),
        (st_within(F("location"), Point::new(0.0, 0.0)), "ST_Within("),
        (
            st_intersects(F("location"), Point::new(0.0, 0.0)),
            "ST_Intersects(",
        ),
    ] {
        let stmt = Postgres.compile_update(&update_set(build)).unwrap();
        assert!(stmt.sql.contains(label), "PG {label}: {}", stmt.sql);
    }
}

// ---------- MySQL / SQLite rejection ----------

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_st_distance() {
    let q = update_set(st_distance(F("location"), Point::new(1.0, 2.0)));
    let err = Sqlite.compile_update(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("ST_Distance"), "op label: {op}");
            assert_eq!(dialect, "sqlite");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_st_dwithin() {
    let q = update_set(st_dwithin(
        F("location"),
        Point::new(1.0, 2.0),
        Expr::Literal(SqlValue::F64(0.5)),
    ));
    let err = MySql.compile_update(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("ST_DWithin")
    ));
}

// ---------- Arity checks ----------

#[test]
fn st_dwithin_with_two_args_arity_mismatch() {
    let bad = Expr::Function {
        kind: ScalarFn::StDWithin,
        args: vec![F("location").into(), Point::new(0.0, 0.0).into()],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "ST_DWithin",
            ..
        }
    ));
}

#[test]
fn st_distance_with_three_args_arity_mismatch() {
    let bad = Expr::Function {
        kind: ScalarFn::StDistance,
        args: vec![
            F("location").into(),
            Point::new(0.0, 0.0).into(),
            Expr::Literal(SqlValue::F64(1.0)),
        ],
    };
    let err = Postgres.compile_update(&update_set(bad)).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "ST_Distance",
            ..
        }
    ));
}
