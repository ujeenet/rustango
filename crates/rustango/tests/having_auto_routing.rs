//! Tri-dialect emission tests for HAVING auto-routing (issue #74).
//! `AggregateBuilder::filter(field, op, value)` routes to HAVING when
//! `field` matches an annotation alias from a prior `.annotate(...)`;
//! else forwards to WHERE. HAVING is SQL-92 standard — identical
//! emission across PG / MySQL / SQLite (only placeholder + ident
//! quote differ).

use rustango::core::aggregates::{count_all, sum};
use rustango::core::Op;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "har_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    author_id: i64,
    #[rustango(max_length = 20)]
    status: String,
    price: i64,
}

// ---------- WHERE-only: filter against model column ----------

#[test]
fn filter_on_model_column_routes_to_where() {
    let q = Post::objects()
        .aggregate()
        .annotate("c", count_all().into())
        .filter("status", Op::Eq, "published")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "status" = $1"#),
        "WHERE clause for model column: {}",
        stmt.sql
    );
    assert!(!stmt.sql.contains("HAVING"), "no HAVING: {}", stmt.sql);
}

// ---------- HAVING-only: filter against annotation alias ----------

#[test]
fn filter_on_annotation_alias_routes_to_having() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::Gt, 10_i64)
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"HAVING COUNT(*) > $1"#),
        "HAVING clause for annotation alias: {}",
        stmt.sql
    );
    // No WHERE clause when only annotation filters are present.
    assert!(
        !stmt.sql.contains("WHERE"),
        "no WHERE when only annotation filters: {}",
        stmt.sql
    );
}

// ---------- Both: WHERE + HAVING in same query ----------

#[test]
fn mixed_filter_splits_predicates_across_where_and_having() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("status", Op::Eq, "published") // WHERE
        .filter("post_count", Op::Gt, 10_i64) // HAVING
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "status" = $1"#),
        "WHERE clause present: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#"HAVING COUNT(*) > $2"#),
        "HAVING uses the aggregate expression directly (not the alias — PG requires this): {}",
        stmt.sql
    );
    // WHERE comes BEFORE GROUP BY, HAVING after.
    let where_pos = stmt.sql.find("WHERE").unwrap();
    let group_pos = stmt.sql.find("GROUP BY").unwrap();
    let having_pos = stmt.sql.find("HAVING").unwrap();
    assert!(where_pos < group_pos && group_pos < having_pos);
}

// ---------- Multiple HAVING-routed filters AND-join ----------

#[test]
fn multiple_annotation_filters_and_join_in_having() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .annotate("total_revenue", sum("price").into())
        .filter("post_count", Op::Gt, 10_i64)
        .filter("total_revenue", Op::Gt, 1000_i64)
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // HAVING emits the lifted aggregate expressions, not the SELECT
    // aliases — `COUNT(*) > $1` and `SUM("price")::bigint > $2`
    // (PG widens SUM to NUMERIC; the int-cast wrapper applies even
    // inside HAVING).
    assert!(
        stmt.sql.contains("COUNT(*) > $1") && stmt.sql.contains(" AND "),
        "two annotation filters AND-joined in HAVING (lifted aggregates): {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#"SUM("price")"#),
        "second filter's SUM lifted into HAVING: {}",
        stmt.sql
    );
}

// ---------- Neither: aggregating query with no filter ----------

#[test]
fn aggregate_without_filter_emits_no_where_or_having() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("c", count_all().into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(!stmt.sql.contains("WHERE"));
    assert!(!stmt.sql.contains("HAVING"));
}

// ---------- Chain order: annotate must precede filter ----------

#[test]
fn filter_before_annotate_routes_to_where_not_having() {
    // Order matters in v1: filter() at call time checks the current
    // annotation registry. If filter("c", ...) runs BEFORE the
    // corresponding annotate("c", ...), it routes to WHERE — and
    // the resolve_pending validator catches "c" as not a model
    // column.
    let r = Post::objects()
        .aggregate()
        .filter("post_count", Op::Gt, 10_i64) // routes to WHERE (alias not yet registered)
        .annotate("post_count", count_all().into())
        .compile();
    // WHERE-side validator surfaces UnknownField since "post_count"
    // is not a real model column.
    assert!(
        matches!(
            r,
            Err(rustango::core::QueryError::UnknownField { ref field, .. }) if field == "post_count"
        ),
        "filter-before-annotate should surface UnknownField at compile, got: {r:?}",
    );
}

// ---------- Tri-dialect: HAVING is uniform ----------

#[test]
fn mysql_emits_having_with_backticks() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::Gt, 10_i64)
        .compile()
        .unwrap();
    let stmt = MySql.compile_aggregate(&q).unwrap();
    // Aggregate expression lifted into HAVING — MySQL spelling.
    assert!(
        stmt.sql.contains("HAVING COUNT(*) > ?"),
        "MySQL: HAVING aggregate-expression form: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_emits_having_with_double_quotes() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::Gt, 10_i64)
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("HAVING COUNT(*) > ?"),
        "SQLite: HAVING aggregate-expression form: {}",
        stmt.sql
    );
}

// ---------- Richer ops on alias-routed filter (issue #87) ----------

/// `Op::In` against an annotation alias emits `HAVING <agg> IN ($1, $2, …)`.
/// Pre-#87 this rejected at `compile()` with `HavingOpNotSupported`.
#[test]
fn op_in_against_alias_emits_having_in() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter(
            "post_count",
            Op::In,
            SqlValue::List(vec![SqlValue::I64(5), SqlValue::I64(10), SqlValue::I64(20)]),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("HAVING COUNT(*) IN ($1, $2, $3)"),
        "PG: HAVING aggregate IN (...): {}",
        stmt.sql
    );
    assert_eq!(stmt.params.len(), 3);
}

#[test]
fn op_not_in_against_alias_emits_having_not_in() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter(
            "post_count",
            Op::NotIn,
            SqlValue::List(vec![SqlValue::I64(0), SqlValue::I64(1)]),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("HAVING COUNT(*) NOT IN ($1, $2)"),
        "PG: HAVING aggregate NOT IN (...): {}",
        stmt.sql
    );
}

#[test]
fn op_between_against_alias_emits_having_between() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter(
            "post_count",
            Op::Between,
            SqlValue::List(vec![SqlValue::I64(5), SqlValue::I64(10)]),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("HAVING COUNT(*) BETWEEN $1 AND $2"),
        "PG: HAVING aggregate BETWEEN ...: {}",
        stmt.sql
    );
    assert_eq!(stmt.params.len(), 2);
}

#[test]
fn op_isnull_against_alias_emits_having_is_null() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::IsNull, SqlValue::Bool(true))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("HAVING COUNT(*) IS NULL"),
        "PG: HAVING aggregate IS NULL: {}",
        stmt.sql
    );
    // No params for IS NULL.
    assert!(stmt.params.is_empty());
}

#[test]
fn op_isnull_false_against_alias_emits_having_is_not_null() {
    use rustango::core::SqlValue;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::IsNull, SqlValue::Bool(false))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("HAVING COUNT(*) IS NOT NULL"),
        "PG: HAVING aggregate IS NOT NULL: {}",
        stmt.sql
    );
}

#[test]
fn op_like_against_alias_emits_having_like() {
    use rustango::core::aggregates::max;
    // MAX(name) returns a string — LIKE pattern makes sense.
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter("max_status", Op::Like, "publish%")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"HAVING MAX("status") LIKE $1"#),
        "PG: HAVING aggregate LIKE pattern: {}",
        stmt.sql
    );
}

#[test]
fn op_not_like_against_alias_emits_having_not_like() {
    use rustango::core::aggregates::max;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter("max_status", Op::NotLike, "draft%")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"HAVING MAX("status") NOT LIKE $1"#),
        "PG: HAVING aggregate NOT LIKE pattern: {}",
        stmt.sql
    );
}

#[test]
fn op_ilike_against_alias_emits_pg_native_ilike() {
    use rustango::core::aggregates::max;
    // PG: native ILIKE — `MAX("status") ILIKE $1`.
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter("max_status", Op::ILike, "PUBLISH%")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"HAVING MAX("status") ILIKE $1"#),
        "PG: HAVING aggregate ILIKE pattern: {}",
        stmt.sql
    );
}

#[test]
fn op_ilike_against_alias_falls_back_to_lower_on_mysql() {
    use rustango::core::aggregates::max;
    // MySQL has no ILIKE; the writer falls back to LOWER(...) LIKE LOWER(?).
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter("max_status", Op::ILike, "PUBLISH%")
        .compile()
        .unwrap();
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains("HAVING LOWER(MAX(`status`)) LIKE LOWER(?)"),
        "MySQL: HAVING LOWER(agg) LIKE LOWER(?): {}",
        stmt.sql
    );
}

#[test]
fn op_ilike_against_alias_falls_back_to_lower_on_sqlite() {
    use rustango::core::aggregates::max;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter("max_status", Op::ILike, "PUBLISH%")
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"HAVING LOWER(MAX("status")) LIKE LOWER(?)"#),
        "SQLite: HAVING LOWER(agg) LIKE LOWER(?): {}",
        stmt.sql
    );
}

/// Param-order sanity: MySQL uses positional `?`. The lhs aggregate may
/// carry inner literals (e.g. via `Filtered`); those must bind BEFORE the
/// `LIKE`/`ILIKE` rhs literal so positional placeholders match the
/// param vector textually.
#[test]
fn ilike_alias_param_order_preserved_for_positional_dialects() {
    use rustango::core::aggregates::max;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter("status", Op::Eq, "published") // WHERE — first param
        .filter("max_status", Op::ILike, "PUB%") // HAVING — second param
        .compile()
        .unwrap();
    let stmt = MySql.compile_aggregate(&q).unwrap();
    // Two params total; WHERE first, HAVING ILIKE last.
    assert_eq!(stmt.params.len(), 2);
    // The `=` placeholder for WHERE appears before the `LIKE LOWER(...)`
    // placeholder for HAVING in the SQL text.
    let where_pos = stmt.sql.find("`status` = ?").unwrap();
    let having_pos = stmt.sql.find("LIKE LOWER(?)").unwrap();
    assert!(where_pos < having_pos);
}

/// JSON ops + IsDistinctFrom are still rejected — they need dialect-
/// specific writers that take a `&str` for the LHS.
#[test]
fn json_op_against_alias_still_rejected_at_compile() {
    use rustango::core::aggregates::max;
    use rustango::core::SqlValue;
    let r = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("max_status", max("status").into())
        .filter(
            "max_status",
            Op::JsonContains,
            SqlValue::Json(serde_json::json!({"key": "value"})),
        )
        .compile();
    match r {
        Err(rustango::core::QueryError::HavingOpNotSupported { alias, op }) => {
            assert_eq!(alias, "max_status");
            assert_eq!(op, Op::JsonContains);
        }
        other => panic!("expected HavingOpNotSupported, got {other:?}"),
    }
}

#[test]
fn is_distinct_from_against_alias_still_rejected_at_compile() {
    let r = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::IsDistinctFrom, 5_i64)
        .compile();
    assert!(matches!(
        r,
        Err(rustango::core::QueryError::HavingOpNotSupported {
            op: Op::IsDistinctFrom,
            ..
        })
    ));
}

/// Same op + non-alias field still routes to WHERE — the WHERE path
/// supports the full Op set as before (LIKE on a model string column
/// is the canonical use). Op-validation is HAVING-specific.
#[test]
fn op_like_against_model_column_still_works_via_where() {
    let q = Post::objects()
        .aggregate()
        .annotate("c", count_all().into())
        .filter("status", Op::Like, "publish%")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "status" LIKE $1"#),
        "WHERE LIKE on model column still works: {}",
        stmt.sql
    );
}

/// Once an error is recorded, subsequent builder calls are no-ops so
/// the original cause isn't masked by downstream complaints.
#[test]
fn deferred_error_swallows_subsequent_builder_calls() {
    let r = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .filter("post_count", Op::IsDistinctFrom, 5_i64) // sets deferred error
        .filter("status", Op::Eq, "published") // should NOT overwrite
        .filter("post_count", Op::Gt, 10_i64) // should NOT overwrite
        .compile();
    match r {
        Err(rustango::core::QueryError::HavingOpNotSupported {
            op: Op::IsDistinctFrom,
            ..
        }) => {}
        other => panic!("expected the FIRST error to survive, got {other:?}"),
    }
}

// ---------- HAVING composes with explicit .having() call ----------

#[test]
fn auto_routed_filter_composes_with_explicit_having() {
    // `.having()` (typed) + `.filter(alias, ...)` (string-keyed) both
    // land in `having`. They AND-join.
    use rustango::core::Column as _;
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        .having(Post::price.gt(100_i64)) // typed → goes to having (model column)
        .filter("post_count", Op::Gt, 10_i64) // alias → also having
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    // .having() puts a typed `Post::price.gt(100)` predicate into
    // having (rendered as the column ref); the auto-routed
    // .filter("post_count", ...) puts the lifted COUNT(*) expression.
    assert!(
        stmt.sql.contains("HAVING")
            && stmt.sql.contains(r#""price" > $1"#)
            && stmt.sql.contains("COUNT(*) > $2")
            && stmt.sql.contains(" AND "),
        "explicit .having() + auto-routed filter AND-join: {}",
        stmt.sql
    );
}
