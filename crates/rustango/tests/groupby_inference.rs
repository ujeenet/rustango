//! Tri-dialect emission tests for Django-shape GROUP BY auto-inference
//! (issue #75). The inference rule:
//!   * `.values(cols).annotate(agg)` → `GROUP BY cols` (Shape 2)
//!   * `.annotate(agg)` alone        → `GROUP BY` every scalar column (Shape 3)
//!   * `.annotate(window)` only      → no GROUP BY (window funcs are per-row)
//!   * Explicit `.group_by(...)`     → always wins; values still drives nothing
//!     (we trust the explicit list).

use rustango::core::aggregates::{count_all, sum};
use rustango::core::window::row_number;
use rustango::core::{Column as _, QueryError};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "gbi_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    author_id: i64,
    #[rustango(max_length = 20)]
    status: String,
    views: i64,
    revenue: i64,
}

// ---------- Shape 2: .values(cols).annotate(agg) → GROUP BY cols ----------

#[test]
fn values_then_aggregate_emits_group_by_values() {
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"SELECT "author_id""#),
        "projection: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#"GROUP BY "author_id""#),
        "group by: {}",
        stmt.sql
    );
}

#[test]
fn values_multi_column_groups_by_all_listed_cols() {
    let q = Post::objects()
        .values(&["author_id", "status"])
        .annotate("revenue_total", sum("revenue").into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"GROUP BY "author_id", "status""#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn values_with_filter_emits_where_before_group_by() {
    let q = Post::objects()
        .where_(Post::status.eq("published".to_owned()))
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    let wh = stmt.sql.find("WHERE").unwrap();
    let gb = stmt.sql.find("GROUP BY").unwrap();
    assert!(wh < gb, "WHERE before GROUP BY: {}", stmt.sql);
}

// ---------- Shape 3: bare .annotate(agg) → GROUP BY all scalar cols ----------

#[test]
fn bare_annotate_groups_by_every_scalar_column() {
    let q = Post::objects()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    for col in ["id", "author_id", "status", "views", "revenue"] {
        assert!(
            stmt.sql.contains(&format!(r#""{col}""#)),
            "Shape 3 must include every scalar col in GROUP BY — missing `{col}`: {}",
            stmt.sql
        );
    }
    assert!(stmt.sql.contains("GROUP BY"), "got: {}", stmt.sql);
}

// ---------- Scalar aggregate via .aggregate().annotate(): NO GROUP BY ----------

#[test]
fn aggregate_then_annotate_stays_scalar() {
    // Regression pin: the rustango-native `.aggregate().annotate(agg)` path
    // must remain a scalar single-row aggregate. Adding Shape 3 inference
    // to this path broke every pre-existing aggregate-filter live test
    // (PR #82) and the entire aggregations cookbook chapter.
    let q = Post::objects()
        .aggregate()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        !stmt.sql.contains("GROUP BY"),
        ".aggregate().annotate(agg) must NOT emit GROUP BY (scalar aggregate): {}",
        stmt.sql
    );
}

// ---------- Window-only: no GROUP BY ----------

#[test]
fn window_only_annotate_emits_no_group_by() {
    let q = Post::objects()
        .aggregate()
        // Window functions are per-row; no GROUP BY required.
        .annotate("rn", row_number().order_by(&[("views", true)]).into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        !stmt.sql.contains("GROUP BY"),
        "window-only must not emit GROUP BY: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains("ROW_NUMBER()"), "got: {}", stmt.sql);
}

// ---------- Explicit .group_by(...) wins ----------

#[test]
fn explicit_group_by_overrides_values() {
    // User asked .values("author_id") but also .group_by("status") —
    // we trust the explicit list and emit GROUP BY status (not author_id).
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .group_by("status")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"GROUP BY "status""#),
        "explicit wins: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(r#"GROUP BY "author_id""#),
        "values list should not leak into GROUP BY when explicit was set: {}",
        stmt.sql
    );
}

#[test]
fn explicit_group_by_skips_shape_3_inference() {
    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Postgres.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"GROUP BY "author_id""#),
        "got: {}",
        stmt.sql
    );
    // Other scalar cols must NOT have crept in.
    assert!(
        !stmt.sql.contains(r#"GROUP BY "id", "author_id""#),
        "Shape 3 fallback should not fire when group_by is explicit: {}",
        stmt.sql
    );
}

// ---------- Error paths ----------

#[test]
fn values_alone_without_aggregate_errors() {
    let r = Post::objects().values(&["author_id"]).compile();
    match r {
        Err(QueryError::ValuesRequiresAggregate { cols }) => {
            assert_eq!(cols, vec!["author_id"]);
        }
        other => panic!("expected ValuesRequiresAggregate, got: {other:?}"),
    }
}

#[test]
fn values_with_only_window_annotation_errors() {
    // Window doesn't aggregate — same path as no aggregate at all.
    let r = Post::objects()
        .values(&["author_id"])
        .annotate("rn", row_number().order_by(&[("views", true)]).into())
        .compile();
    assert!(matches!(r, Err(QueryError::ValuesRequiresAggregate { .. })));
}

#[test]
fn values_with_unknown_column_errors() {
    let r = Post::objects()
        .values(&["nope_col"])
        .annotate("n", count_all().into())
        .compile();
    match r {
        Err(QueryError::UnknownField { field, .. }) => assert_eq!(field, "nope_col"),
        other => panic!("expected UnknownField, got: {other:?}"),
    }
}

#[test]
fn explicit_group_by_unknown_column_errors() {
    let r = Post::objects()
        .aggregate()
        .group_by("nope_col")
        .annotate("n", count_all().into())
        .compile();
    match r {
        Err(QueryError::UnknownField { field, .. }) => assert_eq!(field, "nope_col"),
        other => panic!("expected UnknownField, got: {other:?}"),
    }
}

// ---------- Tri-dialect ident-quote shapes ----------

#[test]
fn mysql_backticks_in_values_path() {
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = MySql.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains("`author_id`") && stmt.sql.contains("GROUP BY `author_id`"),
        "MySQL shape: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_double_quotes_in_values_path() {
    let q = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_aggregate(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""author_id""#) && stmt.sql.contains(r#"GROUP BY "author_id""#),
        "SQLite shape: {}",
        stmt.sql
    );
}

#[test]
fn mysql_backticks_in_shape3_path() {
    let q = Post::objects()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = MySql.compile_aggregate(&q).unwrap();
    // Sanity — MySQL backticks present + GROUP BY contains every scalar col.
    for col in ["id", "author_id", "status", "views", "revenue"] {
        assert!(
            stmt.sql.contains(&format!("`{col}`")),
            "MySQL Shape 3 missing `{col}`: {}",
            stmt.sql
        );
    }
}

#[test]
fn sqlite_shape3_includes_all_cols() {
    let q = Post::objects()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_aggregate(&q).unwrap();
    for col in ["id", "author_id", "status", "views", "revenue"] {
        assert!(
            stmt.sql.contains(&format!(r#""{col}""#)),
            "SQLite Shape 3 missing \"{col}\": {}",
            stmt.sql
        );
    }
}

// ---------- QuerySet::annotate shortcut ----------

#[test]
fn queryset_annotate_is_django_shape_aggregate_is_scalar() {
    // The two entry points are intentionally distinct (Django's
    // `aggregate()` vs `annotate()` distinction):
    //   * QuerySet::annotate(...)         → Django Shape 3 — GROUP BY all cols
    //   * QuerySet::aggregate().annotate  → scalar single-row aggregate (no GROUP BY)
    let django_shape = Post::objects()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let scalar = Post::objects()
        .aggregate()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let s1 = Postgres.compile_aggregate(&django_shape).unwrap().sql;
    let s2 = Postgres.compile_aggregate(&scalar).unwrap().sql;
    assert!(
        s1.contains("GROUP BY"),
        "QuerySet::annotate should auto-infer Shape 3 GROUP BY: {s1}"
    );
    assert!(
        !s2.contains("GROUP BY"),
        "QuerySet::aggregate().annotate stays scalar (no GROUP BY): {s2}"
    );
}
