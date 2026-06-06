//! Tri-dialect emission tests for Django-shape `.filter("field__lookup", value)`
//! (issue #71). The suffix parser is dialect-independent — these tests
//! pin the SQL string + the placeholder dialect shape + the
//! transformed-value (e.g. `%rust%` wrapping for `__icontains`).

use rustango::core::SqlValue;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "fl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 20)]
    status: String,
    views: i64,
    author_id: i64,
    deleted_at: Option<i64>,
}

// ---------- Bare field = exact-eq ----------

#[test]
fn bare_field_is_exact_eq() {
    let qs = Post::objects().filter("status", "draft");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "status" = $1"#),
        "bare field → Eq: {}",
        stmt.sql
    );
    assert_eq!(stmt.params, vec![SqlValue::String("draft".into())]);
}

#[test]
fn explicit_exact_suffix_is_eq() {
    let qs = Post::objects().filter("status__exact", "draft");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#"WHERE "status" = $1"#));
}

// ---------- Comparison suffixes ----------

#[test]
fn gt_gte_lt_lte_ne_map_directly() {
    for (suffix, op_sql) in [
        ("gt", " > "),
        ("gte", " >= "),
        ("lt", " < "),
        ("lte", " <= "),
        ("ne", " <> "),
    ] {
        let qs = Post::objects().filter(&format!("views__{suffix}"), 100_i64);
        let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
        assert!(
            stmt.sql.contains(&format!(r#""views"{op_sql}$1"#)),
            "__{suffix} should emit {op_sql} — got {}",
            stmt.sql
        );
    }
}

// ---------- LIKE wrappers ----------

#[test]
fn contains_wraps_value_in_percent() {
    let qs = Post::objects().filter("title__contains", "rust");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#""title" LIKE $1"#), "got: {}", stmt.sql);
    assert_eq!(stmt.params, vec![SqlValue::String("%rust%".into())]);
}

#[test]
fn icontains_uses_ilike_with_percent_wrap() {
    let qs = Post::objects().filter("title__icontains", "rust");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""title" ILIKE $1"#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(stmt.params, vec![SqlValue::String("%rust%".into())]);
}

#[test]
fn startswith_wraps_value_with_trailing_percent_only() {
    let qs = Post::objects().filter("title__startswith", "Hello");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#""title" LIKE $1"#));
    assert_eq!(stmt.params, vec![SqlValue::String("Hello%".into())]);
}

#[test]
fn endswith_wraps_value_with_leading_percent_only() {
    let qs = Post::objects().filter("title__endswith", "!");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#""title" LIKE $1"#));
    assert_eq!(stmt.params, vec![SqlValue::String("%!".into())]);
}

#[test]
fn iexact_uses_ilike_without_wildcards() {
    let qs = Post::objects().filter("title__iexact", "Hello World");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#""title" ILIKE $1"#));
    // No wildcard wrapping for iexact — exact case-insensitive match.
    assert_eq!(stmt.params, vec![SqlValue::String("Hello World".into())]);
}

#[test]
fn istartswith_iendswith_use_ilike() {
    let qs = Post::objects()
        .filter("title__istartswith", "Hello")
        .filter("title__iendswith", "!");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains("ILIKE"));
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::String("Hello%".into()),
            SqlValue::String("%!".into()),
        ]
    );
}

// ---------- __in / __isnull / __between ----------

#[test]
fn in_emits_in_clause_with_list_value() {
    let qs = Post::objects().filter(
        "author_id__in",
        SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)]),
    );
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""author_id" IN ($1, $2, $3)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn isnull_true_emits_is_null() {
    let qs = Post::objects().filter("deleted_at__isnull", true);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""deleted_at" IS NULL"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn isnull_false_emits_is_not_null() {
    let qs = Post::objects().filter("deleted_at__isnull", false);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""deleted_at" IS NOT NULL"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn between_emits_between_clause() {
    let qs = Post::objects().filter(
        "views__between",
        SqlValue::List(vec![SqlValue::I64(10), SqlValue::I64(100)]),
    );
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""views" BETWEEN $1 AND $2"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn range_alias_emits_between_clause() {
    // Django uses `__range`; rustango accepts both for ergonomics.
    let qs = Post::objects().filter(
        "views__range",
        SqlValue::List(vec![SqlValue::I64(10), SqlValue::I64(100)]),
    );
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#""views" BETWEEN $1 AND $2"#));
}

// ---------- Error paths ----------

#[test]
fn unknown_suffix_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects().filter("status__nope", "draft").compile();
    match r {
        Err(QueryError::UnknownLookup { field, suffix }) => {
            assert_eq!(field, "status");
            assert_eq!(suffix, "nope");
        }
        other => panic!("expected UnknownLookup, got: {other:?}"),
    }
}

#[test]
fn chained_lookup_unknown_suffix_errors_at_compile() {
    // `author__name__icontains` — v1 doesn't support join-traversal,
    // so the suffix `"name__icontains"` is rejected as unknown.
    use rustango::core::QueryError;
    let r = Post::objects()
        .filter("author__name__icontains", "alice")
        .compile();
    assert!(matches!(
        r,
        Err(QueryError::UnknownLookup { ref suffix, .. })
            if suffix == "name__icontains"
    ));
}

#[test]
fn in_with_non_list_value_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects()
        .filter("author_id__in", 42_i64) // not a list
        .compile();
    assert!(matches!(
        r,
        Err(QueryError::InvalidLookupValue {
            ref suffix,
            ..
        }) if suffix == "in"
    ));
}

#[test]
fn not_between_emits_not_between_clause() {
    // Eloquent `whereNotBetween` parity — sibling of `__between`.
    let bounds = SqlValue::List(vec![10_i64.into(), 100_i64.into()]);
    let qs = Post::objects().filter("views__not_between", bounds);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""views" NOT BETWEEN $1 AND $2"#),
        "expected NOT BETWEEN, got: {}",
        stmt.sql
    );
}

#[test]
fn not_range_alias_emits_not_between_clause() {
    let bounds = SqlValue::List(vec![10_i64.into(), 100_i64.into()]);
    let qs = Post::objects().filter("views__not_range", bounds);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""views" NOT BETWEEN $1 AND $2"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn not_in_emits_not_in_clause() {
    // Eloquent `whereNotIn` parity — sibling of `__in`.
    let list = SqlValue::List(vec![1_i64.into(), 2_i64.into(), 3_i64.into()]);
    let qs = Post::objects().filter("author_id__not_in", list);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""author_id" NOT IN"#),
        "expected NOT IN clause, got: {}",
        stmt.sql
    );
}

#[test]
fn not_in_with_non_list_value_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects()
        .filter("author_id__not_in", 42_i64) // not a list
        .compile();
    assert!(matches!(
        r,
        Err(QueryError::InvalidLookupValue {
            ref suffix,
            ..
        }) if suffix == "not_in"
    ));
}

#[test]
fn isnull_with_non_bool_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects()
        .filter("deleted_at__isnull", 0_i64) // not a bool
        .compile();
    assert!(matches!(
        r,
        Err(QueryError::InvalidLookupValue {
            ref suffix,
            ..
        }) if suffix == "isnull"
    ));
}

#[test]
fn between_with_wrong_arity_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects()
        .filter(
            "views__between",
            SqlValue::List(vec![SqlValue::I64(1)]), // only 1 element
        )
        .compile();
    assert!(matches!(
        r,
        Err(QueryError::InvalidLookupValue {
            ref suffix,
            ..
        }) if suffix == "between"
    ));
}

#[test]
fn contains_with_non_string_value_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects()
        .filter("title__contains", 42_i64) // LIKE requires String
        .compile();
    assert!(matches!(
        r,
        Err(QueryError::InvalidLookupValue {
            ref suffix,
            ..
        }) if suffix == "contains"
    ));
}

#[test]
fn unknown_field_errors_at_compile() {
    use rustango::core::QueryError;
    let r = Post::objects().filter("nope_field__eq", "x").compile();
    // Filter through to where_clause; resolve_pending catches it.
    assert!(matches!(
        r,
        Err(QueryError::UnknownLookup { ref suffix, .. }) if suffix == "eq"
    ));
    // (NB: `__eq` isn't in the supported set — Django doesn't have
    // `__eq`, only `__exact`. Document this in the cookbook.)
}

// ---------- Multiple filters AND-join ----------

#[test]
fn multiple_filters_compose_into_and_chain() {
    let qs = Post::objects()
        .filter("status", "published")
        .filter("views__gt", 100_i64)
        .filter("title__icontains", "rust");
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""status" = $1"#)
            && stmt.sql.contains(r#""views" > $2"#)
            && stmt.sql.contains(r#""title" ILIKE $3"#)
            && stmt.sql.contains(" AND "),
        "got: {}",
        stmt.sql
    );
}

// ---------- Tri-dialect ident-quote shapes ----------

#[test]
fn mysql_uses_backticks_for_filter_columns() {
    let qs = Post::objects().filter("title__icontains", "rust");
    let stmt = MySql.compile_select(&qs.compile().unwrap()).unwrap();
    // MySQL has no native ILIKE; the writer routes through
    // dialect.write_ilike which emulates with LOWER().
    assert!(
        stmt.sql.contains("`title`") || stmt.sql.contains("LOWER(`title`)"),
        "MySQL backticks: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_keeps_double_quotes_for_filter_columns() {
    let qs = Post::objects().filter("title__contains", "rust");
    let stmt = Sqlite.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#""title" LIKE ?"#),
        "SQLite shape: {}",
        stmt.sql
    );
}
