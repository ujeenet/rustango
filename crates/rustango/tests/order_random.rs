//! Tri-dialect emission tests for `.order_random()` — issue #77.
//! PG + SQLite use `RANDOM()`, MySQL uses `RAND()`. The IR has no
//! direction / NULLS slot; the writer emits exactly
//! `ORDER BY <fn>()` (no `DESC`, no `NULLS …`).

use rustango::core::Model as _;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "or_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
}

#[test]
fn pg_order_random_emits_random_function() {
    let stmt = Postgres
        .compile_select(&Post::objects().order_random().compile().unwrap())
        .unwrap();
    assert!(stmt.sql.ends_with("ORDER BY RANDOM()"), "got: {}", stmt.sql);
}

#[test]
fn sqlite_order_random_emits_random_function() {
    let stmt = Sqlite
        .compile_select(&Post::objects().order_random().compile().unwrap())
        .unwrap();
    assert!(stmt.sql.ends_with("ORDER BY RANDOM()"), "got: {}", stmt.sql);
}

#[test]
fn mysql_order_random_emits_rand_function() {
    let stmt = MySql
        .compile_select(&Post::objects().order_random().compile().unwrap())
        .unwrap();
    assert!(stmt.sql.ends_with("ORDER BY RAND()"), "got: {}", stmt.sql);
}

#[test]
fn order_random_composes_after_order_by_column() {
    // `ORDER BY "title", RANDOM()` — random key sorts rows whose
    // title ties. Order-of-builder-calls is preserved.
    let stmt = Postgres
        .compile_select(
            &Post::objects()
                .order_by(&[("title", false)])
                .order_random()
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "title", RANDOM()"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn order_random_emits_no_desc_or_nulls_suffix() {
    // The IR has no direction / NULLS slot for Random; the writer
    // should NOT emit `DESC` or `NULLS …` after `RANDOM()` even when
    // the sibling `.order_by` has those.
    let stmt = Postgres
        .compile_select(&Post::objects().order_random().compile().unwrap())
        .unwrap();
    assert!(
        !stmt.sql.contains("DESC"),
        "no DESC after random: {}",
        stmt.sql
    );
    assert!(!stmt.sql.contains("NULLS"), "no NULLS clause: {}", stmt.sql);
}

#[test]
fn order_random_emits_no_param_bindings() {
    // RANDOM()/RAND() take no arguments — no params bound.
    let stmt = Postgres
        .compile_select(&Post::objects().order_random().compile().unwrap())
        .unwrap();
    assert!(
        stmt.params.is_empty(),
        "order_random binds no params: {:?}",
        stmt.params
    );
}

#[test]
fn replace_order_by_clears_random_item() {
    // `.replace_order_by(&[...])` must clear a prior `.order_random()`
    // call along with column-keyed items — the docstring on
    // replace_order_by promises a clean slate.
    let stmt = Postgres
        .compile_select(
            &Post::objects()
                .order_random()
                .replace_order_by(&[("id", false)])
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert!(
        stmt.sql.ends_with(r#"ORDER BY "id""#),
        "random must be cleared by replace_order_by: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("RANDOM"),
        "random must not survive replace_order_by: {}",
        stmt.sql
    );
}

#[test]
fn flip_order_by_leaves_random_untouched() {
    // `.flip_order_by()` inverts column directions and NULLS
    // positions. Random has neither — it should pass through as-is.
    let stmt = Postgres
        .compile_select(
            &Post::objects()
                .order_by(&[("title", false)])
                .order_random()
                .flip_order_by()
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert!(
        stmt.sql.contains(r#"ORDER BY "title" DESC, RANDOM()"#),
        "title flipped to DESC, RANDOM() unchanged: {}",
        stmt.sql
    );
}
