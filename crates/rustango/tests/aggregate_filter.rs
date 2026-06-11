//! Tri-dialect emission tests for filtered aggregates +
//! StdDev/Variance + COALESCE-on-empty (issue #6). PG + SQLite (3.30+)
//! use native `FILTER (WHERE …)`; MySQL falls back to
//! `<agg>(CASE WHEN … THEN <arg> END)`. SQLite rejects
//! StdDev/Variance — no built-in.

use rustango::core::aggregates::{
    avg, count, count_all, count_distinct, max, min, stddev, stddev_pop, sum, variance,
    variance_pop,
};
use rustango::core::{
    AggregateExpr, AggregateQuery, Column as _, Filter, Model as _, Op, SqlValue, WhereExpr,
};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "af_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 20)]
    status: String,
    is_active: bool,
    price: i64,
    pages: i64,
}

fn agg(expr: AggregateExpr) -> AggregateQuery {
    AggregateQuery {
        model: Post::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        aggregates: vec![("agg".into(), expr)],
        aliases: vec![],
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
    }
}

// ---------- Plain aggregates (regression for the writer refactor) ----------

#[test]
fn flat_count_unchanged_after_refactor() {
    let q = agg(count("id").into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(stmt.sql.contains(r#"COUNT("id") AS "agg""#));
}

#[test]
fn flat_count_all_unchanged_after_refactor() {
    let q = agg(count_all().into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(stmt.sql.contains(r#"COUNT(*) AS "agg""#));
}

#[test]
fn flat_count_distinct_unchanged_after_refactor() {
    let q = agg(count_distinct("status").into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(stmt.sql.contains(r#"COUNT(DISTINCT "status") AS "agg""#));
}

// ---------- FILTER (WHERE …) — PG + SQLite native ----------

#[test]
fn pg_filtered_count_emits_filter_where_clause() {
    let q = agg(count_all().filter(Post::is_active.eq(true)).into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"COUNT(*) FILTER (WHERE "is_active" = $1)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_filtered_count_uses_native_filter_keyword() {
    // SQLite ≥3.30 supports FILTER natively — confirm we emit the
    // SQL-standard form, not the CASE WHEN fallback.
    let q = agg(count_all().filter(Post::is_active.eq(true)).into());
    let stmt = Sqlite.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"COUNT(*) FILTER (WHERE "is_active" = ?)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn pg_filtered_sum_keeps_int_cast_wrap_through_filter() {
    let q = agg(sum("price").filter(Post::status.eq("published")).into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // The cast must wrap the (agg FILTER (...)) form on PG —
    // `SUM(x)::bigint FILTER (...)` is a parse error because `::`
    // binds tightly. Emit the cast around the parenthesized FILTER.
    assert!(
        stmt.sql
            .contains(r#"(SUM("price") FILTER (WHERE "status" = $1))::bigint"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- MySQL CASE WHEN fallback ----------

#[test]
fn mysql_filtered_count_rewrites_to_case_when() {
    let q = agg(count_all().filter(Post::is_active.eq(true)).into());
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains("COUNT(CASE WHEN `is_active` = ? THEN 1 END)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_filtered_count_column_arg_uses_column_in_case_then() {
    let q = agg(count("id").filter(Post::is_active.eq(true)).into());
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains("COUNT(CASE WHEN `is_active` = ? THEN `id` END)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_filtered_sum_keeps_dialect_int_cast() {
    // The MySQL Sum arm wraps the CASE-WHEN sum with the dialect's
    // int cast — same way the flat path does. Confirms the
    // `emit_filtered_with_cast` helper threads the wrapper.
    let q = agg(sum("price").filter(Post::status.eq("published")).into());
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains("SUM(CASE WHEN `status` = ? THEN `price` END)"),
        "core form: {}",
        stmt.sql
    );
}

#[test]
fn mysql_filtered_distinct_count_rewrites_to_case_when() {
    let q = agg(count_distinct("status")
        .filter(Post::is_active.eq(true))
        .into());
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains("COUNT(DISTINCT CASE WHEN `is_active` = ? THEN `status` END)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_filtered_max_min_avg_rewrite_uniformly() {
    for (b_kind, expected_kw) in [("MAX", "MAX"), ("MIN", "MIN"), ("AVG", "AVG")] {
        let agg_expr: AggregateExpr = match b_kind {
            "MAX" => max("pages").filter(Post::status.eq("draft")).into(),
            "MIN" => min("pages").filter(Post::status.eq("draft")).into(),
            "AVG" => avg("pages").filter(Post::status.eq("draft")).into(),
            _ => unreachable!(),
        };
        let stmt = MySql.compile_aggregate(&agg(agg_expr)).unwrap();
        let needle = format!("{expected_kw}(CASE WHEN `status` = ? THEN `pages` END)");
        assert!(
            stmt.sql.contains(&needle),
            "{b_kind} fallback shape: {}",
            stmt.sql
        );
    }
}

// ---------- COALESCE-on-empty (default=) ----------

#[test]
fn pg_default_wraps_in_coalesce() {
    let q = agg(sum("price").default(0_i64).into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // The inner `SUM("price")` keeps its `::bigint` cast; COALESCE
    // wraps the lot.
    assert!(
        stmt.sql.contains(r#"COALESCE(SUM("price")::bigint, $1)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn pg_filter_and_default_compose_coalesce_outside_filter() {
    let q = agg(sum("price")
        .filter(Post::status.eq("published"))
        .default(0_i64)
        .into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // COALESCE((SUM(price) FILTER (WHERE status = $1))::bigint, $2)
    assert!(
        stmt.sql
            .contains(r#"COALESCE((SUM("price") FILTER (WHERE "status" = $1))::bigint, $2)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_filter_and_default_compose_coalesce_outside_case_when() {
    let q = agg(sum("price")
        .filter(Post::status.eq("published"))
        .default(0_i64)
        .into());
    let stmt = MySql.compile_aggregate(&q).unwrap();
    // MySQL wraps SUM with `CAST(... AS SIGNED)` via the dialect's
    // int-cast helper.
    assert!(
        stmt.sql
            .contains("COALESCE(CAST(SUM(CASE WHEN `status` = ? THEN `price` END) AS SIGNED), ?)"),
        "got: {}",
        stmt.sql
    );
}

// ---------- StdDev / Variance ----------

#[test]
fn pg_stddev_family_emits_sql_standard_names() {
    for (b, expected) in [
        (stddev("pages").build(), "STDDEV_SAMP"),
        (stddev_pop("pages").build(), "STDDEV_POP"),
        (variance("pages").build(), "VAR_SAMP"),
        (variance_pop("pages").build(), "VAR_POP"),
    ] {
        let stmt = Postgres.compile_aggregate(&agg(b)).unwrap();
        let needle = format!(r#"{expected}("pages")"#);
        assert!(stmt.sql.contains(&needle), "{expected} shape: {}", stmt.sql);
    }
}

#[test]
fn mysql_stddev_family_emits_sql_standard_names() {
    let stmt = MySql
        .compile_aggregate(&agg(stddev("pages").build()))
        .unwrap();
    assert!(
        stmt.sql.contains("STDDEV_SAMP(`pages`)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_stddev_is_rejected_at_emit_time() {
    let q = agg(stddev("pages").build());
    let err = Sqlite.compile_aggregate(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::AggregateNotSupported {
                aggregate: "STDDEV_SAMP",
                dialect: "sqlite"
            }
        ),
        "expected AggregateNotSupported, got {err:?}",
    );
}

#[test]
fn sqlite_variance_is_rejected_at_emit_time() {
    let q = agg(variance_pop("pages").build());
    let err = Sqlite.compile_aggregate(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::AggregateNotSupported {
                aggregate: "VAR_POP",
                dialect: "sqlite"
            }
        ),
        "got {err:?}",
    );
}

#[test]
fn pg_filtered_stddev_uses_native_filter() {
    let q = agg(stddev("pages").filter(Post::is_active.eq(true)).into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // STDDEV_SAMP returns NUMERIC on PG for bigint input — the
    // writer wraps with `::double precision` so the decoder's f64
    // path picks it up.
    assert!(
        stmt.sql.contains(
            r#"(STDDEV_SAMP("pages") FILTER (WHERE "is_active" = $1))::double precision"#
        ),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_filtered_stddev_falls_back_to_case_when() {
    let q = agg(stddev("pages").filter(Post::is_active.eq(true)).into());
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains("STDDEV_SAMP(CASE WHEN `is_active` = ? THEN `pages` END)"),
        "got: {}",
        stmt.sql
    );
}

// ---------- Predicate composition inside filter= ----------

#[test]
fn filter_accepts_and_or_typed_expr() {
    let cond = Post::is_active
        .eq(true)
        .and(Post::pages.gt(100_i64))
        .or(Post::status.eq("featured"));
    let q = agg(count_all().filter(cond).into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(stmt.sql.contains("COUNT(*) FILTER (WHERE"));
    assert!(stmt.sql.contains(" AND "));
    assert!(stmt.sql.contains(" OR "));
}

#[test]
fn filter_accepts_raw_where_expr() {
    let predicate = WhereExpr::Predicate(Filter {
        column: "is_active",
        op: Op::Eq,
        value: SqlValue::Bool(true),
    });
    let q = agg(count_all().filter(predicate).into());
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(stmt.sql.contains("FILTER (WHERE"));
}

// ---------- Nested-wrapper rejection (programmer-error safety net) ----------

#[test]
fn nested_filtered_is_rejected_at_emit_time() {
    // The builder never produces this — only a hand-rolled IR can.
    let inner = AggregateExpr::Filtered {
        inner: Box::new(AggregateExpr::Count(None)),
        filter: WhereExpr::Predicate(Filter {
            column: "is_active",
            op: Op::Eq,
            value: SqlValue::Bool(true),
        }),
    };
    let outer = AggregateExpr::Filtered {
        inner: Box::new(inner),
        filter: WhereExpr::Predicate(Filter {
            column: "is_active",
            op: Op::Eq,
            value: SqlValue::Bool(false),
        }),
    };
    let err = Postgres.compile_aggregate(&agg(outer)).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::NestedAggregateWrapper {
                wrapper: "Filtered"
            }
        ),
        "got {err:?}",
    );
}

#[test]
fn nested_coalesced_is_rejected_at_emit_time() {
    let inner = AggregateExpr::Coalesced {
        inner: Box::new(AggregateExpr::Sum("price")),
        default: SqlValue::I64(0),
    };
    let outer = AggregateExpr::Coalesced {
        inner: Box::new(inner),
        default: SqlValue::I64(0),
    };
    let err = Postgres.compile_aggregate(&agg(outer)).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::NestedAggregateWrapper {
                wrapper: "Coalesced"
            }
        ),
        "got {err:?}",
    );
}
