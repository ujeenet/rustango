//! Tri-dialect SQL-emission tests for `QuerySet` set algebra
//! (issue #25). Django's `.union(other_qs, all=)` /
//! `.intersection(other_qs)` / `.difference(other_qs)` lower to SQL
//! `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`. Postgres + SQLite
//! support all four; MySQL needs 8.0.31+ for INTERSECT/EXCEPT, but
//! the writer emits the same SQL — older MySQL just returns a syntax
//! error from the driver.

use rustango::core::Column as _;
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "salg_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 20)]
    status: String,
    author_id: i64,
}

// ---------- UNION ----------

#[test]
fn union_emits_parenthesized_union_on_pg() {
    let q = Post::objects()
        .where_(Post::status.eq("draft"))
        .union(Post::objects().where_(Post::status.eq("review")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Each branch wrapped in parens, joined with UNION.
    assert!(
        stmt.sql.contains(r#"WHERE "status" = $1"#) && stmt.sql.contains(r#"WHERE "status" = $2"#),
        "two branches: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(") UNION ("),
        "parens + UNION: {}",
        stmt.sql
    );
    assert!(!stmt.sql.contains(") UNION ALL ("));
    // No outer ORDER BY when none set.
    assert!(
        !stmt.sql.contains("ORDER BY"),
        "no spurious ORDER BY: {}",
        stmt.sql
    );
}

#[test]
fn union_all_emits_union_all_keyword() {
    let q = Post::objects()
        .union_all(Post::objects().where_(Post::status.eq("archived")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(") UNION ALL ("),
        "UNION ALL: {}",
        stmt.sql
    );
}

#[test]
fn intersection_emits_intersect_keyword() {
    let q = Post::objects()
        .where_(Post::author_id.eq(1_i64))
        .intersection(Post::objects().where_(Post::status.eq("published")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(") INTERSECT ("),
        "INTERSECT: {}",
        stmt.sql
    );
}

#[test]
fn difference_emits_except_keyword() {
    let q = Post::objects()
        .difference(Post::objects().where_(Post::status.eq("deleted")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.contains(") EXCEPT ("), "EXCEPT: {}", stmt.sql);
}

// ---------- Outer ORDER BY / LIMIT / OFFSET apply to merged result ----------

#[test]
fn outer_order_by_emitted_after_compound() {
    let q = Post::objects()
        .where_(Post::status.eq("draft"))
        .union(Post::objects().where_(Post::status.eq("review")))
        .order_by(&[("id", false)])
        .limit(20)
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Outer ORDER BY comes after the last `)` of the compound.
    let last_paren = stmt.sql.rfind(')').expect("compound has close paren");
    let order_pos = stmt.sql.find("ORDER BY").expect("has ORDER BY");
    let limit_pos = stmt.sql.find("LIMIT").expect("has LIMIT");
    assert!(
        last_paren < order_pos,
        "ORDER BY after compound close: {}",
        stmt.sql
    );
    assert!(order_pos < limit_pos, "ORDER BY before LIMIT: {}", stmt.sql);
}

#[test]
fn branch_order_by_stays_inside_parens() {
    // Branch has its own order_by + limit. Outer compound has no
    // order_by. The branch ORDER BY stays INSIDE the branch's parens
    // (PG/SQLite reject mid-compound ORDER BY otherwise).
    let q = Post::objects()
        .union(
            Post::objects()
                .where_(Post::status.eq("review"))
                .order_by(&[("id", true)])
                .limit(5),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // The branch's `ORDER BY "id" DESC LIMIT 5` lives between two
    // parens, and there's no outer ORDER BY after the last `)`.
    let union_pos = stmt.sql.find(") UNION (").expect("has UNION");
    let last_paren = stmt.sql.rfind(')').unwrap();
    let order_pos = stmt.sql.find("ORDER BY").expect("branch ORDER BY exists");
    assert!(
        order_pos > union_pos && order_pos < last_paren,
        "branch ORDER BY sits inside the second branch's parens: {}",
        stmt.sql
    );
}

// ---------- Multiple branches accumulate ----------

#[test]
fn three_branch_union_emits_three_set_op_keywords() {
    let q = Post::objects()
        .where_(Post::status.eq("a"))
        .union(Post::objects().where_(Post::status.eq("b")))
        .union(Post::objects().where_(Post::status.eq("c")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    let count = stmt.sql.matches(") UNION (").count();
    assert_eq!(count, 2, "two ) UNION ( joins for 3 branches: {}", stmt.sql);
}

#[test]
fn mixed_union_intersection_chain_preserves_order() {
    // (A) UNION (B) INTERSECT (C) — SQL evaluates left-to-right.
    let q = Post::objects()
        .where_(Post::status.eq("a"))
        .union(Post::objects().where_(Post::status.eq("b")))
        .intersection(Post::objects().where_(Post::status.eq("c")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // UNION comes before INTERSECT.
    let union_pos = stmt.sql.find(") UNION (").expect("has UNION");
    let intersect_pos = stmt.sql.find(") INTERSECT (").expect("has INTERSECT");
    assert!(
        union_pos < intersect_pos,
        "UNION before INTERSECT (chain order): {}",
        stmt.sql
    );
}

// ---------- Tri-dialect ----------

#[cfg(feature = "mysql")]
#[test]
fn union_emits_backtick_identifiers_on_mysql() {
    let q = Post::objects()
        .union(Post::objects().where_(Post::status.eq("review")))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(") UNION (") && stmt.sql.contains("`status`"),
        "MySQL UNION + backticks: {}",
        stmt.sql
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn except_emits_on_sqlite() {
    let q = Post::objects()
        .difference(Post::objects().where_(Post::status.eq("deleted")))
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(") EXCEPT ("),
        "SQLite EXCEPT: {}",
        stmt.sql
    );
    // SQLite uses `?` placeholders.
    assert!(stmt.sql.contains("?"), "sqlite placeholders: {}", stmt.sql);
}

// ---------- Default queryset (no compound) — no parens around base ----------

#[test]
fn without_compound_no_parens_added_to_plain_select() {
    let q = Post::objects()
        .where_(Post::status.eq("draft"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        !stmt.sql.starts_with('('),
        "plain SELECT not wrapped: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(" UNION ")
            && !stmt.sql.contains(" INTERSECT ")
            && !stmt.sql.contains(" EXCEPT "),
        "no set-op keywords: {}",
        stmt.sql
    );
}

// ---------- Params accumulate across branches ----------

#[test]
fn each_branch_contributes_its_own_params() {
    // Outer: status = "a". Branch 1: status = "b". Branch 2: id > 100.
    let q = Post::objects()
        .where_(Post::status.eq("a"))
        .union(Post::objects().where_(Post::status.eq("b")))
        .union(Post::objects().where_(Post::id.gt(100_i64)))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert_eq!(
        stmt.params.len(),
        3,
        "three branches × one param each: {:?}",
        stmt.params
    );
    // PG placeholders $1, $2, $3 in textual order.
    assert!(stmt.sql.contains("$1") && stmt.sql.contains("$2") && stmt.sql.contains("$3"));
}
