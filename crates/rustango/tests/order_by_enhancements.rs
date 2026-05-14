//! Tri-dialect emission tests for ORDER BY enhancements (issue #76):
//! NULLS FIRST/LAST handling + Expr items in order_by.
//!
//! PG + SQLite emit the SQL-standard `NULLS …` keyword natively;
//! MySQL has no native NULLS syntax so the writer emulates with an
//! `<col> IS NULL` pre-sort term.

use rustango::core::funcs::lower;
use rustango::core::{Column as _, Model as _, NullsOrder, F};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "ob_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    score: Option<i64>,
}

// ---------- Backward compat: existing .order_by(&[(col, desc)]) ----------

#[test]
fn legacy_order_by_emits_no_nulls_clause_on_pg() {
    let qs = Post::objects().order_by(&[("score", true), ("id", false)]);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "score" DESC, "id""#),
        "default emits no NULLS clause: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("NULLS"),
        "no NULLS keyword without explicit pin: {}",
        stmt.sql
    );
}

#[test]
fn legacy_order_by_unchanged_on_mysql_no_is_null_emulation() {
    let qs = Post::objects().order_by(&[("score", true)]);
    let stmt = MySql.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains("ORDER BY `score` DESC"));
    assert!(
        !stmt.sql.contains("IS NULL"),
        "default ordering on MySQL emits no IS NULL trick: {}",
        stmt.sql
    );
}

// ---------- NULLS FIRST / LAST on PG + SQLite ----------

#[test]
fn pg_nulls_last_emits_nulls_last_keyword() {
    let qs = Post::objects().order_by_with_nulls(&[("score", true, NullsOrder::Last)]);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "score" DESC NULLS LAST"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn pg_nulls_first_on_asc_emits_keyword() {
    let qs = Post::objects().order_by_with_nulls(&[("score", false, NullsOrder::First)]);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "score" NULLS FIRST"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_supports_native_nulls_keyword() {
    // SQLite 3.30+ understands NULLS FIRST/LAST — same emission as PG.
    let qs = Post::objects().order_by_with_nulls(&[("score", true, NullsOrder::Last)]);
    let stmt = Sqlite.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "score" DESC NULLS LAST"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- MySQL: NULLS emulation via `<col> IS NULL` pre-sort ----------

#[test]
fn mysql_nulls_last_emulated_via_is_null_asc_prefix() {
    // `NULLS LAST` → group NULLs to the bottom → `IS NULL ASC` term
    // sorts non-NULLs (IS NULL = 0) before NULLs (IS NULL = 1).
    let qs = Post::objects().order_by_with_nulls(&[("score", true, NullsOrder::Last)]);
    let stmt = MySql.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql
            .contains("ORDER BY `score` IS NULL ASC, `score` DESC"),
        "MySQL NULLS LAST emulation: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("NULLS"),
        "no native NULLS keyword on MySQL: {}",
        stmt.sql
    );
}

#[test]
fn mysql_nulls_first_emulated_via_is_null_desc_prefix() {
    let qs = Post::objects().order_by_with_nulls(&[("score", false, NullsOrder::First)]);
    let stmt = MySql.compile_select(&qs.compile().unwrap()).unwrap();
    // `NULLS FIRST` → group NULLs to the top → `IS NULL DESC` term.
    assert!(
        stmt.sql.contains("ORDER BY `score` IS NULL DESC, `score`"),
        "MySQL NULLS FIRST emulation: {}",
        stmt.sql
    );
}

// ---------- Expr items in ORDER BY ----------

#[test]
fn order_by_expr_lower_title_emits_function_call() {
    let qs = Post::objects().order_by_expr(lower(F("title")), false);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY LOWER("title")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn order_by_expr_with_nulls_combines_both() {
    let qs = Post::objects().order_by_expr_with_nulls(lower(F("title")), true, NullsOrder::Last);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql
            .contains(r#"ORDER BY LOWER("title") DESC NULLS LAST"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn order_by_expr_arithmetic_emits_binop_form() {
    let qs = Post::objects().order_by_expr(F("score") + 1_i64, true);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    // The `score + 1` arithmetic should emit via the standard expr
    // writer (parenthesized BinOp), DESC follows.
    assert!(
        stmt.sql.contains(r#"ORDER BY ("score" + $1) DESC"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Composition: legacy + extras in the same query ----------

#[test]
fn legacy_and_extras_compose_legacy_first_then_extras() {
    let qs = Post::objects()
        .order_by(&[("id", false)])
        .order_by_with_nulls(&[("score", true, NullsOrder::Last)]);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql
            .contains(r#"ORDER BY "id", "score" DESC NULLS LAST"#),
        "legacy entries emit before extras: {}",
        stmt.sql
    );
}

// ---------- replace_order_by clears extras ----------

#[test]
fn replace_order_by_clears_both_legacy_and_extras() {
    let qs = Post::objects()
        .order_by(&[("id", false)])
        .order_by_with_nulls(&[("score", true, NullsOrder::Last)])
        .replace_order_by(&[("title", false)]);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    // Isolate the ORDER BY clause — "score" appears in the SELECT
    // projection too, so a bare contains-check would false-positive.
    let order_clause = stmt
        .sql
        .split("ORDER BY ")
        .nth(1)
        .expect("ORDER BY present");
    assert_eq!(
        order_clause.trim(),
        r#""title""#,
        "extras cleared, only `title` remains in ORDER BY: {}",
        stmt.sql
    );
}

// ---------- flip_order_by inverts both legacy + extras ----------

#[test]
fn flip_order_by_inverts_extras_and_swaps_nulls_first_last() {
    let qs = Post::objects()
        .order_by(&[("id", false)])
        .order_by_with_nulls(&[("score", true, NullsOrder::Last)])
        .flip_order_by();
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    // After flip: id DESC (was ASC), score ASC NULLS FIRST (was DESC NULLS LAST).
    assert!(
        stmt.sql
            .contains(r#"ORDER BY "id" DESC, "score" NULLS FIRST"#),
        "flip inverts desc + swaps First/Last: {}",
        stmt.sql
    );
}

// ---------- Validator: typo in order_by_with_nulls field name ----------

#[test]
fn order_by_with_nulls_typo_caught_at_compile_time() {
    use rustango::core::QueryError;
    let err = Post::objects()
        .order_by_with_nulls(&[("nope_col", true, NullsOrder::Last)])
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, QueryError::UnknownField { ref field, .. } if field == "nope_col"),
        "expected UnknownField, got: {err:?}",
    );
}

// ---------- Dialect capability flag is set correctly ----------

#[test]
fn dialect_capability_pg_supports_nulls() {
    assert!(Postgres.supports_nulls_order());
}

#[test]
fn dialect_capability_sqlite_supports_nulls() {
    assert!(Sqlite.supports_nulls_order());
}

#[test]
fn dialect_capability_mysql_does_not_support_nulls() {
    assert!(!MySql.supports_nulls_order());
}
