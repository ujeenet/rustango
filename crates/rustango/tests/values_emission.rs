//! Tri-dialect emission tests for `QuerySet::values_dict` /
//! `values_list` / `values_list_flat` (issue #22). Pure projection
//! — no GROUP BY — emits `SELECT col1, col2 FROM table …` on every
//! dialect.

use rustango::core::{Column as _, QueryError};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "v_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 64)]
    title: String,
    view_count: i64,
    published: bool,
}

// ---------- PG ----------

#[test]
fn values_dict_emits_select_only_listed_cols_on_pg() {
    let q = Post::objects()
        .values_dict(&["id", "title"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Exactly `SELECT "id", "title"` — no `, "view_count"` etc.
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title" FROM "v_post""#),
        "PG values_dict: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(r#""view_count""#) && !stmt.sql.contains(r#""published""#),
        "PG values_dict should not include unlisted cols: {}",
        stmt.sql
    );
}

#[test]
fn values_list_emits_same_select_as_values_dict_on_pg() {
    // Same projection shape — the SQL doesn't differ between
    // values_dict / values_list. Only the row-decode shape on the
    // fetch side differs.
    let dict_q = Post::objects()
        .values_dict(&["id", "view_count"])
        .compile()
        .unwrap();
    let list_q = Post::objects()
        .values_list(&["id", "view_count"])
        .compile()
        .unwrap();
    let dict_sql = Postgres.compile_select(&dict_q).unwrap().sql;
    let list_sql = Postgres.compile_select(&list_q).unwrap().sql;
    assert_eq!(
        dict_sql, list_sql,
        "values_dict + values_list should compile to the same SQL"
    );
}

#[test]
fn values_list_flat_single_col_select_on_pg() {
    let q = Post::objects().values_list_flat("title").compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.starts_with(r#"SELECT "title" FROM "v_post""#),
        "PG values_list_flat: {}",
        stmt.sql
    );
}

#[test]
fn values_dict_preserves_where_clause_on_pg() {
    let q = Post::objects()
        .where_(Post::published.eq(true))
        .values_dict(&["id", "title"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title" FROM "v_post""#),
        "projection: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#"WHERE "published" = $1"#),
        "where clause survives: {}",
        stmt.sql
    );
}

#[test]
fn values_dict_preserves_order_limit_offset_on_pg() {
    let q = Post::objects()
        .order_by(&[("view_count", true)])
        .limit(10)
        .offset(20)
        .values_dict(&["id"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "view_count" DESC"#),
        "{}",
        stmt.sql
    );
    assert!(stmt.sql.contains("LIMIT 10"), "{}", stmt.sql);
    assert!(stmt.sql.contains("OFFSET 20"), "{}", stmt.sql);
}

#[test]
fn values_preserves_column_order_in_projection() {
    // Reorder: title first, id second — the SELECT list must reflect
    // the user's order, not the model's declaration order.
    let q = Post::objects()
        .values_dict(&["title", "id"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "title", "id" FROM "v_post""#),
        "{}",
        stmt.sql
    );
}

// ---------- MySQL ----------

#[cfg(feature = "mysql")]
#[test]
fn values_dict_emits_backtick_quoted_select_on_mysql() {
    let q = Post::objects()
        .values_dict(&["id", "title"])
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.starts_with("SELECT `id`, `title` FROM `v_post`"),
        "MySQL: {}",
        stmt.sql
    );
}

// ---------- SQLite ----------

#[cfg(feature = "sqlite")]
#[test]
fn values_dict_emits_double_quoted_select_on_sqlite() {
    let q = Post::objects()
        .values_dict(&["id", "title"])
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .starts_with(r#"SELECT "id", "title" FROM "v_post""#),
        "SQLite: {}",
        stmt.sql
    );
}

// ---------- Validation ----------

#[test]
fn empty_cols_rejected_at_compile() {
    let err = Post::objects().values_dict(&[]).compile().unwrap_err();
    assert!(
        matches!(err, QueryError::EmptyValuesProjection),
        "got: {err:?}"
    );
    let err = Post::objects().values_list(&[]).compile().unwrap_err();
    assert!(
        matches!(err, QueryError::EmptyValuesProjection),
        "got: {err:?}"
    );
}

#[test]
fn unknown_column_rejected_at_compile() {
    let err = Post::objects()
        .values_dict(&["id", "nope_col"])
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_col"),
        "got: {err:?}"
    );
}

#[test]
fn values_list_flat_unknown_col_rejected() {
    let err = Post::objects()
        .values_list_flat("nope")
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope"),
        "got: {err:?}"
    );
}

// ---------- Backward compat: existing `.values()` AggregateBuilder path unchanged ----------

#[test]
fn existing_values_aggregate_path_still_errors_when_no_annotate() {
    // Sanity check the pre-existing error path still fires —
    // `.values()` (the AggregateBuilder shortcut) without a
    // subsequent `.annotate()` should still surface
    // `ValuesRequiresAggregate`, not the new EmptyValuesProjection
    // / projection path.
    let err = Post::objects().values(&["id"]).compile().unwrap_err();
    assert!(
        matches!(err, QueryError::ValuesRequiresAggregate { .. }),
        "got: {err:?}"
    );
}
