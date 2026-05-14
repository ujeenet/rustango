//! Tri-dialect emission tests for ad-hoc joins (issue #80). The
//! standard JOIN keyword + ON predicate is SQL-92 — emission is
//! identical across PG / MySQL / SQLite for `INNER` and `LEFT`; the
//! divergent cases are `RIGHT` (no SQLite) and `FULL OUTER` (PG only).

use rustango::core::joins::aliased;
use rustango::core::{Expr, Filter, Join, JoinKind, Model as _, Op, SqlValue, WhereExpr};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "aj_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 20)]
    status: String,
}

#[derive(Model)]
#[rustango(table = "aj_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    id: i64,
    post_id: i64,
    #[rustango(max_length = 500)]
    body: String,
    is_approved: bool,
}

// Helper: the `INNER JOIN comment c ON c.post_id = post.id` join,
// pre-built so each test composes from a known shape.
fn inner_post_comment() -> Join {
    Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    }
}

// ---------- Per-kind emission ----------

#[test]
fn inner_join_emits_keyword_and_aliased_on_predicate() {
    let stmt = Postgres
        .compile_select(
            &Post::objects()
                .join(inner_post_comment())
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert!(
        stmt.sql
            .contains(r#"INNER JOIN "aj_comment" AS "c" ON "c"."post_id" = "aj_post"."id""#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn left_join_default_kind_is_left() {
    // Bypass the helper to exercise the default — `JoinKind::default()`.
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::default(),
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains(r#"LEFT JOIN "aj_comment" AS "c""#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn right_join_emits_right_keyword_on_pg_and_mysql() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Right,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let qs = Post::objects().join(join).compile().unwrap();
    let pg = Postgres.compile_select(&qs).unwrap();
    assert!(pg.sql.contains("RIGHT JOIN"), "PG: {}", pg.sql);
    let my = MySql.compile_select(&qs).unwrap();
    assert!(my.sql.contains("RIGHT JOIN"), "MySQL: {}", my.sql);
}

#[test]
fn right_join_is_rejected_on_sqlite() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Right,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let qs = Post::objects().join(join).compile().unwrap();
    let err = Sqlite.compile_select(&qs).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::JoinKindNotSupported {
                kind: "RIGHT",
                dialect: "sqlite"
            }
        ),
        "expected JoinKindNotSupported, got {err:?}",
    );
}

#[test]
fn full_join_emits_full_outer_on_pg() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Full,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(stmt.sql.contains("FULL OUTER JOIN"), "got: {}", stmt.sql);
}

#[test]
fn full_join_is_rejected_on_mysql_and_sqlite() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Full,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let qs = Post::objects().join(join).compile().unwrap();
    let err_my = MySql.compile_select(&qs).unwrap_err();
    assert!(matches!(
        err_my,
        SqlError::JoinKindNotSupported {
            kind: "FULL",
            dialect: "mysql"
        }
    ));
    let err_sq = Sqlite.compile_select(&qs).unwrap_err();
    assert!(matches!(
        err_sq,
        SqlError::JoinKindNotSupported {
            kind: "FULL",
            dialect: "sqlite"
        }
    ));
}

// ---------- ON predicate composition ----------

#[test]
fn on_predicate_composes_column_equality_with_literal_filter() {
    // INNER JOIN comment AS c ON c.post_id = post.id AND c.is_approved = true
    // — the literal-side filter ("is_approved = true") uses a bare
    // Filter whose column qualifies to the joined alias `c` because
    // the writer passes `qualify_with: Some(join.alias)`.
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::And(vec![
            WhereExpr::ExprCompare {
                lhs: aliased("c", "post_id"),
                op: Op::Eq,
                rhs: aliased("aj_post", "id"),
            },
            WhereExpr::Predicate(Filter {
                column: "is_approved",
                op: Op::Eq,
                value: SqlValue::Bool(true),
            }),
        ]),
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains(r#""c"."post_id" = "aj_post"."id""#),
        "join-side equality: {}",
        stmt.sql
    );
    assert!(
        stmt.sql.contains(r#""c"."is_approved" = $1"#),
        "Filter column qualifies to join alias: {}",
        stmt.sql
    );
}

// ---------- Cross-table column qualification in ON ----------

#[test]
fn aliased_helper_emits_explicit_table_prefix() {
    // Confirm aliased("c", "post_id") emits `"c"."post_id"` literally
    // (no scope-stack lookup; pure explicit qualification).
    let e: Expr = aliased("c", "post_id");
    assert_eq!(
        e,
        Expr::AliasedColumn {
            alias: "c",
            column: "post_id",
        },
    );
}

// ---------- Multi-join: select_related + ad-hoc combine ----------

#[test]
fn ad_hoc_join_emits_after_fk_select_related() {
    // select_related is FK-only; this test uses two ad-hoc joins to
    // confirm ordering — the second JOIN appears AFTER the first in
    // emitted SQL.
    let join_c = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let join_c2 = Join {
        target: Comment::SCHEMA,
        alias: "c2",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c2", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(
            &Post::objects()
                .join(join_c)
                .join(join_c2)
                .compile()
                .unwrap(),
        )
        .unwrap();
    let i1 = stmt.sql.find(r#"AS "c""#).expect("c first");
    let i2 = stmt.sql.find(r#"AS "c2""#).expect("c2 second");
    assert!(i1 < i2, "join order preserved (c before c2): {}", stmt.sql);
}

// ---------- Tri-dialect ident-quote shape ----------

#[test]
fn mysql_uses_backticks_for_aliases_and_columns() {
    let stmt = MySql
        .compile_select(
            &Post::objects()
                .join(inner_post_comment())
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert!(
        stmt.sql.contains("INNER JOIN `aj_comment` AS `c`"),
        "MySQL backticks: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains("`c`.`post_id` = `aj_post`.`id`"));
}

#[test]
fn sqlite_uses_double_quotes_like_pg() {
    let stmt = Sqlite
        .compile_select(
            &Post::objects()
                .join(inner_post_comment())
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert!(
        stmt.sql
            .contains(r#"INNER JOIN "aj_comment" AS "c" ON "c"."post_id" = "aj_post"."id""#),
        "SQLite shape: {}",
        stmt.sql
    );
}
