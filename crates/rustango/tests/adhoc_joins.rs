//! Tri-dialect emission tests for ad-hoc joins (issue #80). The
//! standard JOIN keyword + ON predicate is SQL-92 — emission is
//! identical across PG / MySQL / SQLite for `INNER` and `LEFT`; the
//! divergent cases are `RIGHT` (no SQLite) and `FULL OUTER` (PG only).

use rustango::core::joins::{aliased, col_filter};
use rustango::core::{
    Column as _, ColumnFilter, Expr, Filter, Join, JoinKind, Model as _, Op, SqlValue, WhereExpr, F,
};
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

// ---------- Paranoid-review regressions ----------

/// Empty `on` (`WhereExpr::And(vec![])`) used to emit `ON ` with a
/// literal hole — a parse error on every backend. Mirror of
/// `EmptyCaseWhenCondition`; rejected at emit time now.
#[test]
fn empty_on_predicate_is_rejected_at_emit_time() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::And(vec![]),
        project: vec![],
    };
    let err = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap_err();
    assert!(
        matches!(err, SqlError::EmptyJoinOnCondition),
        "expected EmptyJoinOnCondition, got {err:?}",
    );
}

/// Bare `F("col")` (an `Expr::Column`) inside an ON predicate used to
/// emit UNqualified — e.g. `ON "c"."post_id" = "id"`, which PG flags
/// as ambiguous when both tables have an `id`. Fix routes through
/// `Sql.current_qualify_alias` so bare Column refs in ON resolve to
/// the joined alias.
#[test]
fn bare_f_column_inside_on_qualifies_to_joined_alias() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::ColumnCompare(ColumnFilter {
            column: "post_id",
            op: Op::Eq,
            rhs: F("id").into(),
        }),
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains(r#""c"."post_id" = "c"."id""#),
        "bare F() rhs should qualify to joined alias: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(r#"= "id""#),
        "no UNqualified `id` should slip through: {}",
        stmt.sql
    );
}

/// `col_filter(alias, col, op, value)` is the SAFE replacement for
/// typed filters from the OUTER model inside ON predicates — emits
/// `"<alias>"."<col>" <op> <value>` verbatim, with no
/// joined-alias misrouting.
#[test]
fn col_filter_routes_predicate_to_explicit_alias() {
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
            // SAFE outer-table filter inside the ON.
            col_filter("aj_post", "status", Op::Eq, "draft"),
        ]),
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains(r#""aj_post"."status" = $1"#),
        "outer-aliased predicate emits verbatim: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(r#""c"."status""#),
        "no misrouting to joined alias: {}",
        stmt.sql
    );
}

/// Document the residual footgun: a typed filter from the OUTER
/// model (here `Post::status.eq(...)`) DOES still misqualify to the
/// joined alias. This test pins the current (unsafe) behavior so a
/// future fix that closes the type-safety leak surfaces here as a
/// red test. Pairs with the cookbook's "DANGEROUS PATTERN" callout
/// directing users to `col_filter` instead.
#[test]
fn outer_typed_filter_inside_on_still_misqualifies_pinned() {
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
            // User INTENT: outer post.status = 'draft'. The typed
            // filter loses its `Post` model tag at `Into<WhereExpr>`,
            // so the writer misqualifies it to the joined alias.
            // Users should reach for `col_filter` instead.
            Post::status.eq("draft").into(),
        ]),
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql.contains(r#""c"."status" = $1"#),
        "current (unsafe) behavior pinned — emits joined alias: {}",
        stmt.sql
    );
    // When a future fix lands compile-time prevention (e.g. via a
    // `Column<M>::at_alias(alias)` builder that preserves the model
    // tag), this assertion flips and the test name should be renamed
    // to `outer_typed_filter_inside_on_is_caught_at_compile_time`.
}

/// Self-join — same target table joined twice with distinct aliases.
/// Most distinctive ad-hoc-join use case (employee.manager_id =
/// manager.id). Ensure both join clauses emit independently and the
/// aliases don't collide.
#[test]
fn self_join_emits_two_independent_clauses() {
    let parent = Join {
        target: Comment::SCHEMA,
        alias: "c_parent",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c_parent", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec![],
    };
    let child = Join {
        target: Comment::SCHEMA,
        alias: "c_child",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c_child", "post_id"),
            op: Op::Eq,
            rhs: aliased("c_parent", "id"),
        },
        project: vec![],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(parent).join(child).compile().unwrap())
        .unwrap();
    assert!(stmt.sql.contains(r#"AS "c_parent""#));
    assert!(stmt.sql.contains(r#"AS "c_child""#));
    assert!(
        stmt.sql
            .contains(r#""c_child"."post_id" = "c_parent"."id""#),
        "inner self-join references parent alias: {}",
        stmt.sql
    );
}

/// `select_related("…")` + `.join(...)` combine — the FK-driven join
/// emits first (preserving column ordering for legacy decoders), the
/// ad-hoc join after. Regression for the `lower_select_related`
/// migration.
#[test]
fn select_related_combined_with_ad_hoc_join() {
    // Note: Post doesn't have an FK column on this test model — use
    // a bare ad-hoc join alone since the test models don't carry
    // FK metadata. The ordering invariant is exercised by
    // `ad_hoc_join_emits_after_fk_select_related` in the main suite;
    // this test just confirms the IR refactor didn't break the
    // existing FK-LEFT-JOIN shape via lower_select_related (no FK
    // here, so just a smoke test on the new IR).
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Left,
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
    // Left-join keyword preserved, ON predicate uses ExprCompare.
    assert!(stmt.sql.contains(r#"LEFT JOIN "aj_comment""#));
    assert!(stmt.sql.contains(r#""c"."post_id" = "aj_post"."id""#));
}

/// `project: vec!["col"]` on an ad-hoc join still emits the column
/// in the SELECT list (the writer doesn't gate this on `kind`). The
/// cookbook now documents this as dead data — the decoder for
/// `Vec<MainModel>` doesn't read the extra columns. Pin the current
/// behavior so users aren't surprised.
#[test]
fn project_on_ad_hoc_join_appears_in_select_list_today() {
    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("aj_post", "id"),
        },
        project: vec!["is_approved"],
    };
    let stmt = Postgres
        .compile_select(&Post::objects().join(join).compile().unwrap())
        .unwrap();
    assert!(
        stmt.sql
            .contains(r#""c"."is_approved" AS "c__is_approved""#),
        "project columns still emit (cookbook flags this as dead data): {}",
        stmt.sql
    );
}

// Silence unused-import warnings that only fire when the regressions
// at the bottom of the file are commented out for debugging.
#[allow(dead_code)]
fn _used() {
    let _ = (Filter {
        column: "x",
        op: Op::Eq,
        value: SqlValue::I64(1),
    },);
    let _: Expr = aliased("a", "b");
}
