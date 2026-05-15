//! Tri-dialect SQL-emission tests for `WhereExpr::Xor` (issue #27).
//! Django 4.1+ added `Q(a) ^ Q(b)` — "odd number of operands evaluate
//! to true". Native logical XOR exists on MySQL but not PG / SQLite,
//! so the writer emits a portable rewrite uniformly:
//! - 2 children → `(a AND NOT b) OR (NOT a AND b)` canonical form.
//! - 3+ children → CASE-WHEN tally `% 2 = 1` (Django's odd-parity).

use rustango::core::{Column as _, Filter, Model as _, Op, SelectQuery, SqlValue, WhereExpr};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "qxor_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 20)]
    name: String,
    age: i32,
    active: bool,
}

fn select(where_clause: WhereExpr) -> SelectQuery {
    SelectQuery {
        model: User::SCHEMA,
        where_clause,
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
    }
}

// ---------- Binary XOR — canonical (a AND NOT b) OR (NOT a AND b) form ----------

#[test]
fn binary_xor_emits_canonical_form_on_pg() {
    let q = select(WhereExpr::Xor(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
    ]));
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(
            r#"("name" = $1 AND NOT ("active" = $2)) OR (NOT ("name" = $3) AND "active" = $4)"#
        ),
        "PG: canonical binary XOR rewrite: {}",
        stmt.sql
    );
    // Each operand binds once per occurrence in the rewrite (4 params total).
    assert_eq!(stmt.params.len(), 4);
}

#[test]
fn binary_xor_emits_canonical_form_on_mysql() {
    let q = select(WhereExpr::Xor(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
    ]));
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .contains("(`name` = ? AND NOT (`active` = ?)) OR (NOT (`name` = ?) AND `active` = ?)"),
        "MySQL: canonical binary XOR rewrite (backticks): {}",
        stmt.sql
    );
}

#[test]
fn binary_xor_emits_canonical_form_on_sqlite() {
    let q = select(WhereExpr::Xor(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
    ]));
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(
            r#"("name" = ? AND NOT ("active" = ?)) OR (NOT ("name" = ?) AND "active" = ?)"#
        ),
        "SQLite: canonical binary XOR rewrite: {}",
        stmt.sql
    );
}

// ---------- N-ary XOR — Django's odd-parity tally ----------

#[test]
fn ternary_xor_emits_parity_tally_on_pg() {
    let q = select(WhereExpr::Xor(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
        User::age.gt(30_i32).into(),
    ]));
    let stmt = Postgres.compile_select(&q).unwrap();
    // (CASE WHEN q1 THEN 1 ELSE 0 END) + (CASE WHEN q2 …) + (CASE WHEN q3 …) % 2 = 1
    assert!(
        stmt.sql.contains(
            r#"((CASE WHEN "name" = $1 THEN 1 ELSE 0 END) + (CASE WHEN "active" = $2 THEN 1 ELSE 0 END) + (CASE WHEN "age" > $3 THEN 1 ELSE 0 END)) % 2 = 1"#
        ),
        "PG: ternary XOR parity tally: {}",
        stmt.sql
    );
    assert_eq!(stmt.params.len(), 3);
}

#[test]
fn quaternary_xor_emits_four_term_parity_tally() {
    let q = select(WhereExpr::Xor(vec![
        User::name.eq("a").into(),
        User::name.eq("b").into(),
        User::name.eq("c").into(),
        User::name.eq("d").into(),
    ]));
    let stmt = Postgres.compile_select(&q).unwrap();
    // Verify the join shape — four CASE-WHEN tally with three " + " separators.
    let case_count = stmt.sql.matches("CASE WHEN").count();
    let plus_count = stmt.sql.matches(" + ").count();
    assert_eq!(case_count, 4, "four CASE WHEN tally: {}", stmt.sql);
    assert_eq!(plus_count, 3, "three '+' separators: {}", stmt.sql);
    assert!(
        stmt.sql.contains(") % 2 = 1"),
        "ends with `% 2 = 1`: {}",
        stmt.sql
    );
}

// ---------- Degenerate shapes ----------

#[test]
fn single_element_xor_emits_just_the_element() {
    let q = select(WhereExpr::Xor(vec![User::name.eq("alice").into()]));
    let stmt = Postgres.compile_select(&q).unwrap();
    // XOR over one operand is just the operand itself; no rewrite layer.
    assert!(
        stmt.sql.contains(r#"WHERE "name" = $1"#),
        "single-element XOR → unwrapped: {}",
        stmt.sql
    );
    assert!(!stmt.sql.contains("CASE WHEN"));
    assert!(!stmt.sql.contains(" AND NOT "));
}

#[test]
fn empty_xor_branch_returns_named_writer_error() {
    let q = select(WhereExpr::Xor(vec![]));
    let err = Postgres.compile_select(&q).unwrap_err();
    assert!(matches!(err, SqlError::EmptyXorBranch));
}

// ---------- Composition with And / Or / Not / nested Xor ----------

#[test]
fn xor_inside_and_parenthesizes_correctly() {
    // (name = alice XOR active = true) AND age > 30
    let xor = WhereExpr::Xor(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
    ]);
    let where_clause = WhereExpr::And(vec![
        xor,
        WhereExpr::Predicate(Filter {
            column: "age",
            op: Op::Gt,
            value: SqlValue::I32(30),
        }),
    ]);
    let stmt = Postgres.compile_select(&select(where_clause)).unwrap();
    // The XOR rewrite is wrapped in parens by write_child when it
    // appears as a member of an AND, then the AND adds the age check.
    assert!(stmt.sql.contains(") AND "));
    assert!(
        stmt.sql.contains(r#""age" > $5"#),
        "age check follows XOR: {}",
        stmt.sql
    );
}

#[test]
fn xor_inside_or_parenthesizes_correctly() {
    // (name = alice XOR active = true) OR age > 30
    let xor = WhereExpr::Xor(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
    ]);
    let where_clause = WhereExpr::Or(vec![
        xor,
        WhereExpr::Predicate(Filter {
            column: "age",
            op: Op::Gt,
            value: SqlValue::I32(30),
        }),
    ]);
    let stmt = Postgres.compile_select(&select(where_clause)).unwrap();
    assert!(stmt.sql.contains(") OR "));
    assert!(stmt.sql.contains(r#""age" > $5"#));
}

// ---------- TypedExpr::xor() builder API ----------

#[test]
fn typed_expr_xor_method_produces_xor_node() {
    let q = User::objects()
        .where_(User::name.eq("alice").xor(User::active.eq(true)))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(
            r#"("name" = $1 AND NOT ("active" = $2)) OR (NOT ("name" = $3) AND "active" = $4)"#
        ),
        "TypedExpr::xor() → canonical binary XOR rewrite: {}",
        stmt.sql
    );
}

/// Chained `.xor()` flattens into a single N-ary `Xor` node (mirrors
/// how `.and()` / `.or()` flatten). The result emits the parity tally,
/// not a nested binary rewrite — Django's "odd number of trues"
/// semantic for N-ary XOR.
#[test]
fn chained_xor_flattens_to_nary_parity_tally() {
    let q = User::objects()
        .where_(
            User::name
                .eq("alice")
                .xor(User::active.eq(true))
                .xor(User::age.gt(30_i32)),
        )
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    // Three CASE WHEN terms, not a nested binary rewrite.
    let case_count = stmt.sql.matches("CASE WHEN").count();
    assert_eq!(case_count, 3, "flattened ternary XOR: {}", stmt.sql);
    assert!(stmt.sql.contains(") % 2 = 1"));
}

/// N-ary parity tally must parenthesize composite children. Without
/// the `write_child` wrap, a child like `And(a, b)` would emit raw
/// `a AND b` inside `CASE WHEN a AND b THEN 1 ELSE 0 END`. SQL
/// operator precedence (`NOT` > `AND` > `OR`) makes that parse
/// correctly today, but the wrap is belt-and-suspenders against
/// future precedence-ladder changes.
#[test]
fn nary_parity_tally_parenthesizes_composite_children() {
    // Xor of [And(a, b), c, d] — first child is a composite.
    let and_child = WhereExpr::And(vec![
        User::name.eq("alice").into(),
        User::active.eq(true).into(),
    ]);
    let where_clause = WhereExpr::Xor(vec![
        and_child,
        User::age.gt(30_i32).into(),
        User::age.lt(50_i32).into(),
    ]);
    let stmt = Postgres.compile_select(&select(where_clause)).unwrap();
    // Composite first child appears wrapped: `CASE WHEN (a AND b) THEN`
    assert!(
        stmt.sql
            .contains(r#"CASE WHEN ("name" = $1 AND "active" = $2) THEN"#),
        "composite child wrapped in parens inside CASE WHEN: {}",
        stmt.sql
    );
    // Predicate leaf children are NOT wrapped (write_child writes
    // them bare): `CASE WHEN "age" > $3 THEN`
    assert!(
        stmt.sql.contains(r#"CASE WHEN "age" > $3 THEN"#),
        "predicate leaf children emit bare: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains(") % 2 = 1"));
}

/// `WhereExpr::validate()` walks into `Xor` children — proves the
/// `And | Or | Xor` validation arm catches typo'd columns inside an
/// Xor node. Important for any caller that does invoke validate()
/// (e.g., aggregate `Filtered { filter }` runs it on the inner
/// predicate, where an Xor could legitimately appear).
#[test]
fn xor_validate_walks_children() {
    let xor = WhereExpr::Xor(vec![
        WhereExpr::Predicate(Filter {
            column: "name",
            op: Op::Eq,
            value: SqlValue::String("alice".into()),
        }),
        WhereExpr::Predicate(Filter {
            column: "nonexistent_column",
            op: Op::Eq,
            value: SqlValue::Bool(true),
        }),
    ]);
    let r = xor.validate(User::SCHEMA);
    assert!(
        matches!(
            r,
            Err(rustango::core::QueryError::UnknownField { ref field, .. })
                if field == "nonexistent_column"
        ),
        "validate() walks into Xor children: {r:?}",
    );
}
