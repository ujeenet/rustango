//! Tri-dialect SQL-emission tests for `QuerySet` set algebra
//! (issue #25 / #329). Django's `.union(other_qs, all=)` /
//! `.intersection(other_qs)` / `.difference(other_qs)` lower to SQL
//! `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`. Postgres + SQLite
//! support all four; MySQL needs 8.0.31+ for INTERSECT/EXCEPT, but
//! the writer emits the same SQL — older MySQL just returns a syntax
//! error from the driver.
//!
//! The writer emits the **bare** compound shape — `SELECT … UNION
//! SELECT …` — which is portable across all three dialects (SQLite's
//! `compound-select-stmt` grammar forbids parenthesizing select-cores).
//! Branches that carry their own `ORDER BY` / `LIMIT` / `OFFSET` get
//! wrapped in a derived-table `SELECT * FROM (<branch>)` so those
//! clauses scope correctly to the branch instead of attaching to
//! the outer compound.

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
fn union_emits_bare_union_on_pg() {
    let q = Post::objects()
        .where_(Post::status.eq("draft"))
        .union(Post::objects().where_(Post::status.eq("review")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Both branches present with their own params.
    assert!(
        stmt.sql.contains(r#"WHERE "status" = $1"#) && stmt.sql.contains(r#"WHERE "status" = $2"#),
        "two branches: {}",
        stmt.sql
    );
    // Bare UNION between branches (no parens — portable across PG / MySQL / SQLite).
    assert!(stmt.sql.contains(" UNION "), "bare UNION: {}", stmt.sql);
    assert!(!stmt.sql.contains(" UNION ALL "));
    // The whole statement starts with SELECT, not '(' — SQLite hard rule.
    assert!(
        stmt.sql.starts_with("SELECT"),
        "no leading paren: {}",
        stmt.sql
    );
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
    assert!(stmt.sql.contains(" UNION ALL "), "UNION ALL: {}", stmt.sql);
}

#[test]
fn intersection_emits_intersect_keyword() {
    let q = Post::objects()
        .where_(Post::author_id.eq(1_i64))
        .intersection(Post::objects().where_(Post::status.eq("published")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.contains(" INTERSECT "), "INTERSECT: {}", stmt.sql);
}

#[test]
fn difference_emits_except_keyword() {
    let q = Post::objects()
        .difference(Post::objects().where_(Post::status.eq("deleted")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.contains(" EXCEPT "), "EXCEPT: {}", stmt.sql);
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
    // Outer ORDER BY comes after the last UNION, before LIMIT.
    let union_pos = stmt.sql.rfind(" UNION ").expect("has UNION");
    let order_pos = stmt.sql.find("ORDER BY").expect("has ORDER BY");
    let limit_pos = stmt.sql.find("LIMIT").expect("has LIMIT");
    assert!(
        union_pos < order_pos,
        "ORDER BY after final UNION: {}",
        stmt.sql
    );
    assert!(order_pos < limit_pos, "ORDER BY before LIMIT: {}", stmt.sql);
}

#[test]
fn branch_order_by_wraps_in_derived_table() {
    // Branch has its own order_by + limit. Without scoping the
    // ORDER BY/LIMIT would attach to the whole compound — the writer
    // wraps such branches in `SELECT * FROM (<branch>)` to keep
    // those clauses local to the branch on every dialect.
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
    // The branch is wrapped: `... UNION SELECT * FROM (... ORDER BY "id" DESC LIMIT 5)`.
    assert!(
        stmt.sql.contains("UNION SELECT * FROM ("),
        "branch with ORDER BY wraps in derived table: {}",
        stmt.sql
    );
    // ORDER BY + LIMIT live INSIDE the parens, between UNION and the
    // closing paren — they belong to the branch, not the outer compound.
    let union_pos = stmt.sql.find("UNION SELECT * FROM (").unwrap();
    let last_paren = stmt.sql.rfind(')').unwrap();
    let order_pos = stmt.sql.find("ORDER BY").expect("branch ORDER BY exists");
    let limit_pos = stmt.sql.find("LIMIT").expect("branch LIMIT exists");
    assert!(
        order_pos > union_pos && order_pos < last_paren,
        "branch ORDER BY inside derived table: {}",
        stmt.sql
    );
    assert!(
        limit_pos > union_pos && limit_pos < last_paren,
        "branch LIMIT inside derived table: {}",
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
    let count = stmt.sql.matches(" UNION ").count();
    assert_eq!(count, 2, "two UNIONs join 3 branches: {}", stmt.sql);
}

#[test]
fn mixed_union_intersection_chain_preserves_order() {
    // A UNION B INTERSECT C — SQL evaluates left-to-right.
    let q = Post::objects()
        .where_(Post::status.eq("a"))
        .union(Post::objects().where_(Post::status.eq("b")))
        .intersection(Post::objects().where_(Post::status.eq("c")))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    let union_pos = stmt.sql.find(" UNION ").expect("has UNION");
    let intersect_pos = stmt.sql.find(" INTERSECT ").expect("has INTERSECT");
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
        stmt.sql.contains(" UNION ") && stmt.sql.contains("`status`"),
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
    assert!(stmt.sql.contains(" EXCEPT "), "SQLite EXCEPT: {}", stmt.sql);
    // SQLite uses `?` placeholders.
    assert!(stmt.sql.contains("?"), "sqlite placeholders: {}", stmt.sql);
    // SQLite hard rule: the outermost statement must start with SELECT,
    // not `(`. The bare compound shape satisfies this.
    assert!(
        stmt.sql.starts_with("SELECT"),
        "no leading paren: {}",
        stmt.sql
    );
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

// ---------- MySQL: INTERSECT / EXCEPT emission (8.0.31+) ----------

#[cfg(feature = "mysql")]
#[test]
fn intersect_emits_with_backticks_on_mysql() {
    let q = Post::objects()
        .where_(Post::status.eq("published"))
        .intersection(Post::objects().where_(Post::author_id.eq(1_i64)))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(" INTERSECT "),
        "MySQL INTERSECT keyword: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("`status`") && stmt.sql.contains("`author_id`"),
        "MySQL backtick identifiers in both branches: {}",
        stmt.sql
    );
}

#[cfg(feature = "mysql")]
#[test]
fn except_emits_with_backticks_on_mysql() {
    let q = Post::objects()
        .difference(Post::objects().where_(Post::status.eq("deleted")))
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(" EXCEPT "),
        "MySQL EXCEPT keyword: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains("`status`"),
        "MySQL backticks: {}",
        stmt.sql
    );
}

// ---------- Nested compound — one branch is itself a compound ----------

/// `qs_outer.union(qs_inner_compound)` where the inner is itself a
/// compound `qs_a.union(qs_b)`. The recursive `write_compound_select`
/// renders the inner compound wrapped in a derived-table subquery so
/// its `UNION` operators stay scoped — without scoping the inner
/// UNIONs would flatten into the outer compound and lose the nesting
/// semantic.
#[test]
fn nested_compound_inside_outer_union_wraps_in_subquery() {
    let inner = Post::objects()
        .where_(Post::status.eq("a"))
        .union(Post::objects().where_(Post::status.eq("b")));
    let outer = Post::objects()
        .where_(Post::status.eq("c"))
        .union(inner)
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&outer).unwrap();
    // Outer: `SELECT … UNION SELECT * FROM (<inner-compound>)`.
    assert!(
        stmt.sql.contains("UNION SELECT * FROM ("),
        "nested compound wrapped in derived table: {}",
        stmt.sql
    );
    // Three WHERE clauses total (one per branch).
    let where_count = stmt.sql.matches("WHERE").count();
    assert_eq!(where_count, 3, "three branches: {}", stmt.sql);
    // Two UNION joins: outer and inner.
    let union_count = stmt.sql.matches(" UNION ").count();
    assert_eq!(union_count, 2, "two UNIONs: {}", stmt.sql);
}

// ---------- with_compound — fallible entry point ----------

#[test]
fn with_compound_takes_precompiled_branch() {
    use rustango::core::SetOp;
    // Pre-compile the branch so the caller could `?` on errors.
    let branch = Post::objects()
        .where_(Post::status.eq("draft"))
        .compile()
        .unwrap();
    let q = Post::objects()
        .where_(Post::status.eq("published"))
        .with_compound(SetOp::Difference, branch)
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(" EXCEPT "),
        "with_compound(Difference, …): {}",
        stmt.sql
    );
}
