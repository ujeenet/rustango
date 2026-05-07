//! End-to-end check of the `QuerySet` → `SelectQuery` → Postgres SQL pipeline.

use rustango::core::{
    Assignment, BulkInsertQuery, ConflictClause, CountQuery, DeleteQuery, Filter, InsertQuery,
    Join, Model as _, Op, SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use rustango::sql::{Dialect, Postgres, SqlError};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    is_active: bool,
}

#[derive(Model)]
#[rustango(table = "post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    title: String,
    #[rustango(fk = "user", on = "id")]
    author_id: i64,
}

fn pg() -> Postgres {
    Postgres
}

#[test]
fn select_with_no_filters_lists_scalar_columns() {
    let stmt = pg()
        .compile_select(&User::objects().compile().unwrap())
        .unwrap();
    assert_eq!(stmt.sql, r#"SELECT "id", "name", "is_active" FROM "user""#);
    assert!(stmt.params.is_empty());
}

#[test]
fn equality_filter_emits_dollar_placeholder() {
    let stmt = pg()
        .compile_select(&User::objects().eq("name", "alice").compile().unwrap())
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" = $1"#
    );
    assert_eq!(stmt.params, vec![SqlValue::String("alice".into())]);
}

#[test]
fn multiple_filters_join_with_and_and_increment_placeholders() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .eq("name", "alice")
                .filter("is_active", Op::Eq, true)
                .filter("id", Op::Gt, 10_i64)
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" = $1 AND "is_active" = $2 AND "id" > $3"#
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
            SqlValue::I64(10),
        ]
    );
}

#[test]
fn is_null_does_not_consume_placeholder() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .filter("name", Op::IsNull, true)
                .filter("id", Op::Eq, 1_i64)
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" IS NULL AND "id" = $1"#
    );
    assert_eq!(stmt.params, vec![SqlValue::I64(1)]);
}

#[test]
fn is_not_null_emitted_for_false() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .filter("name", Op::IsNull, false)
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "name" IS NOT NULL"#
    );
}

#[test]
fn in_list_expands_to_one_placeholder_per_element() {
    let stmt = pg()
        .compile_select(
            &User::objects()
                .filter(
                    "id",
                    Op::In,
                    SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)]),
                )
                .compile()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "id" IN ($1, $2, $3)"#
    );
    assert_eq!(
        stmt.params,
        vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)],
    );
}

#[test]
fn empty_in_list_is_rejected() {
    let err = pg()
        .compile_select(
            &User::objects()
                .filter("id", Op::In, SqlValue::List(vec![]))
                .compile()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(err, SqlError::EmptyInList));
}

#[test]
fn in_with_non_list_is_rejected() {
    let err = pg()
        .compile_select(
            &User::objects()
                .filter("id", Op::In, 1_i64)
                .compile()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(err, SqlError::InRequiresList));
}

#[test]
fn is_null_with_non_bool_is_rejected() {
    let err = pg()
        .compile_select(
            &User::objects()
                .filter("name", Op::IsNull, "alice")
                .compile()
                .unwrap(),
        )
        .unwrap_err();
    assert!(matches!(err, SqlError::IsNullRequiresBool));
}

#[test]
fn insert_emits_columns_and_placeholders() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name", "is_active"],
        values: vec![
            SqlValue::I64(7),
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
        ],
        returning: Vec::new(),
        on_conflict: None,
    };
    let stmt = pg().compile_insert(&query).unwrap();
    assert_eq!(
        stmt.sql,
        r#"INSERT INTO "user" ("id", "name", "is_active") VALUES ($1, $2, $3)"#,
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::I64(7),
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
        ],
    );
}

#[test]
fn insert_on_conflict_do_nothing() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name"],
        values: vec![SqlValue::I64(1), SqlValue::String("alice".into())],
        returning: Vec::new(),
        on_conflict: Some(ConflictClause::DoNothing),
    };
    let stmt = pg().compile_insert(&query).unwrap();
    assert!(stmt.sql.contains("ON CONFLICT DO NOTHING"), "{}", stmt.sql);
}

#[test]
fn insert_on_conflict_do_update() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name", "is_active"],
        values: vec![
            SqlValue::I64(1),
            SqlValue::String("alice".into()),
            SqlValue::Bool(true),
        ],
        returning: Vec::new(),
        on_conflict: Some(ConflictClause::DoUpdate {
            target: vec!["id"],
            update_columns: vec!["name", "is_active"],
        }),
    };
    let stmt = pg().compile_insert(&query).unwrap();
    assert!(
        stmt.sql.contains(r#"ON CONFLICT ("id") DO UPDATE SET "name" = EXCLUDED."name", "is_active" = EXCLUDED."is_active""#),
        "{}",
        stmt.sql
    );
}

#[test]
fn insert_with_no_columns_is_rejected() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec![],
        values: vec![],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = pg().compile_insert(&query).unwrap_err();
    assert!(matches!(err, SqlError::EmptyInsert));
}

#[test]
fn insert_with_mismatched_lengths_is_rejected() {
    let query = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id"],
        values: vec![SqlValue::I64(1), SqlValue::I64(2)],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = pg().compile_insert(&query).unwrap_err();
    assert!(matches!(
        err,
        SqlError::InsertShapeMismatch {
            columns: 1,
            values: 2
        }
    ));
}

// ---------------- bulk INSERT ----------------

#[test]
fn bulk_insert_emits_one_values_tuple_per_row() {
    let query = BulkInsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name", "is_active"],
        rows: vec![
            vec![
                SqlValue::I64(1),
                SqlValue::String("alice".into()),
                SqlValue::Bool(true),
            ],
            vec![
                SqlValue::I64(2),
                SqlValue::String("bob".into()),
                SqlValue::Bool(false),
            ],
        ],
        returning: Vec::new(),
        on_conflict: None,
    };
    let stmt = pg().compile_bulk_insert(&query).unwrap();
    assert_eq!(
        stmt.sql,
        r#"INSERT INTO "user" ("id", "name", "is_active") VALUES ($1, $2, $3), ($4, $5, $6)"#,
    );
    assert_eq!(stmt.params.len(), 6);
}

#[test]
fn bulk_insert_with_returning_appends_clause() {
    let query = BulkInsertQuery {
        model: User::SCHEMA,
        columns: vec!["name", "is_active"],
        rows: vec![
            vec![SqlValue::String("alice".into()), SqlValue::Bool(true)],
            vec![SqlValue::String("bob".into()), SqlValue::Bool(false)],
        ],
        returning: vec!["id"],
        on_conflict: None,
    };
    let stmt = pg().compile_bulk_insert(&query).unwrap();
    assert!(stmt.sql.ends_with(r#"RETURNING "id""#), "{}", stmt.sql);
}

#[test]
fn bulk_insert_empty_rows_is_rejected() {
    let query = BulkInsertQuery {
        model: User::SCHEMA,
        columns: vec!["name"],
        rows: vec![],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = pg().compile_bulk_insert(&query).unwrap_err();
    assert!(matches!(err, SqlError::EmptyBulkInsert));
}

#[test]
fn bulk_insert_row_shape_mismatch_is_rejected() {
    let query = BulkInsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name"],
        rows: vec![
            vec![SqlValue::I64(1), SqlValue::String("alice".into())],
            vec![SqlValue::I64(2)],
        ],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = pg().compile_bulk_insert(&query).unwrap_err();
    assert!(matches!(err, SqlError::InsertShapeMismatch { .. }));
}

// ---------------- UPDATE ----------------

fn eq_filter(column: &'static str, value: SqlValue) -> Filter {
    Filter {
        column,
        op: Op::Eq,
        value,
    }
}

#[test]
fn update_single_set_no_where_runs_table_wide() {
    let query = UpdateQuery {
        model: User::SCHEMA,
        set: vec![Assignment {
            column: "is_active",
            value: SqlValue::Bool(false),
        }],
        where_clause: WhereExpr::And(vec![]),
    };
    let stmt = pg().compile_update(&query).unwrap();
    assert_eq!(stmt.sql, r#"UPDATE "user" SET "is_active" = $1"#);
    assert_eq!(stmt.params, vec![SqlValue::Bool(false)]);
}

#[test]
fn update_multi_set_with_where_orders_set_then_filter_placeholders() {
    let query = UpdateQuery {
        model: User::SCHEMA,
        set: vec![
            Assignment {
                column: "name",
                value: SqlValue::String("ALICE".into()),
            },
            Assignment {
                column: "is_active",
                value: SqlValue::Bool(false),
            },
        ],
        where_clause: WhereExpr::Predicate(eq_filter("id", SqlValue::I64(7))),
    };
    let stmt = pg().compile_update(&query).unwrap();
    assert_eq!(
        stmt.sql,
        r#"UPDATE "user" SET "name" = $1, "is_active" = $2 WHERE "id" = $3"#,
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::String("ALICE".into()),
            SqlValue::Bool(false),
            SqlValue::I64(7),
        ],
    );
}

#[test]
fn update_with_multiple_filters_chains_with_and() {
    let query = UpdateQuery {
        model: User::SCHEMA,
        set: vec![Assignment {
            column: "is_active",
            value: SqlValue::Bool(true),
        }],
        where_clause: WhereExpr::and_predicates(vec![
            eq_filter("name", SqlValue::String("alice".into())),
            Filter {
                column: "id",
                op: Op::Gt,
                value: SqlValue::I64(0),
            },
        ]),
    };
    let stmt = pg().compile_update(&query).unwrap();
    assert_eq!(
        stmt.sql,
        r#"UPDATE "user" SET "is_active" = $1 WHERE "name" = $2 AND "id" > $3"#,
    );
}

#[test]
fn update_with_empty_set_is_rejected() {
    let query = UpdateQuery {
        model: User::SCHEMA,
        set: vec![],
        where_clause: WhereExpr::Predicate(eq_filter("id", SqlValue::I64(1))),
    };
    let err = pg().compile_update(&query).unwrap_err();
    assert!(matches!(err, SqlError::EmptyUpdateSet));
}

#[test]
fn update_propagates_filter_errors() {
    // `Op::In` with a non-list — same error path that compile_select uses.
    let query = UpdateQuery {
        model: User::SCHEMA,
        set: vec![Assignment {
            column: "is_active",
            value: SqlValue::Bool(false),
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::In,
            value: SqlValue::I64(1),
        }),
    };
    let err = pg().compile_update(&query).unwrap_err();
    assert!(matches!(err, SqlError::InRequiresList));
}

// ---------------- DELETE ----------------

#[test]
fn delete_with_no_filters_runs_table_wide() {
    let query = DeleteQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
    };
    let stmt = pg().compile_delete(&query).unwrap();
    assert_eq!(stmt.sql, r#"DELETE FROM "user""#);
    assert!(stmt.params.is_empty());
}

#[test]
fn delete_with_single_filter() {
    let query = DeleteQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::Predicate(eq_filter("id", SqlValue::I64(42))),
    };
    let stmt = pg().compile_delete(&query).unwrap();
    assert_eq!(stmt.sql, r#"DELETE FROM "user" WHERE "id" = $1"#);
    assert_eq!(stmt.params, vec![SqlValue::I64(42)]);
}

#[test]
fn delete_with_multiple_filters_chains_with_and() {
    let query = DeleteQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::and_predicates(vec![
            eq_filter("name", SqlValue::String("alice".into())),
            eq_filter("is_active", SqlValue::Bool(false)),
        ]),
    };
    let stmt = pg().compile_delete(&query).unwrap();
    assert_eq!(
        stmt.sql,
        r#"DELETE FROM "user" WHERE "name" = $1 AND "is_active" = $2"#,
    );
    assert_eq!(
        stmt.params,
        vec![SqlValue::String("alice".into()), SqlValue::Bool(false)],
    );
}

#[test]
fn delete_with_in_list_expands_placeholders() {
    let query = DeleteQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::In,
            value: SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)]),
        }),
    };
    let stmt = pg().compile_delete(&query).unwrap();
    assert_eq!(stmt.sql, r#"DELETE FROM "user" WHERE "id" IN ($1, $2, $3)"#);
    assert_eq!(
        stmt.params,
        vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)],
    );
}

#[test]
fn delete_with_is_null_does_not_consume_placeholder() {
    let query = DeleteQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "name",
            op: Op::IsNull,
            value: SqlValue::Bool(true),
        }),
    };
    let stmt = pg().compile_delete(&query).unwrap();
    assert_eq!(stmt.sql, r#"DELETE FROM "user" WHERE "name" IS NULL"#);
    assert!(stmt.params.is_empty());
}

#[test]
fn delete_propagates_filter_errors() {
    let query = DeleteQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::In,
            value: SqlValue::List(vec![]),
        }),
    };
    let err = pg().compile_delete(&query).unwrap_err();
    assert!(matches!(err, SqlError::EmptyInList));
}

// ---------------- LIMIT / OFFSET on SelectQuery ----------------

fn empty_select() -> SelectQuery {
    SelectQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
    }
}

#[test]
fn select_emits_limit_when_set() {
    let q = SelectQuery {
        limit: Some(10),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" LIMIT 10"#,
    );
}

#[test]
fn select_emits_offset_when_set() {
    let q = SelectQuery {
        offset: Some(20),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" OFFSET 20"#,
    );
}

#[test]
fn select_emits_both_in_canonical_order() {
    let q = SelectQuery {
        limit: Some(5),
        offset: Some(10),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" LIMIT 5 OFFSET 10"#,
    );
}

#[test]
fn select_with_filters_and_limit_orders_clauses() {
    let q = SelectQuery {
        where_clause: WhereExpr::Predicate(Filter {
            column: "is_active",
            op: Op::Eq,
            value: SqlValue::Bool(true),
        }),
        limit: Some(3),
        offset: Some(0),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "is_active" = $1 LIMIT 3 OFFSET 0"#,
    );
}

// ---------------- COUNT ----------------

#[test]
fn count_with_no_filters() {
    let q = CountQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
    };
    let stmt = pg().compile_count(&q).unwrap();
    assert_eq!(stmt.sql, r#"SELECT COUNT(*) FROM "user""#);
    assert!(stmt.params.is_empty());
}

#[test]
fn count_with_filters() {
    let q = CountQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::and_predicates(vec![
            Filter {
                column: "is_active",
                op: Op::Eq,
                value: SqlValue::Bool(true),
            },
            Filter {
                column: "id",
                op: Op::Gt,
                value: SqlValue::I64(0),
            },
        ]),
    };
    let stmt = pg().compile_count(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT COUNT(*) FROM "user" WHERE "is_active" = $1 AND "id" > $2"#,
    );
    assert_eq!(stmt.params, vec![SqlValue::Bool(true), SqlValue::I64(0)]);
}

#[test]
fn count_propagates_filter_errors() {
    let q = CountQuery {
        model: User::SCHEMA,
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::In,
            value: SqlValue::List(vec![]),
        }),
    };
    let err = pg().compile_count(&q).unwrap_err();
    assert!(matches!(err, SqlError::EmptyInList));
}

// ---------------- SEARCH ----------------

#[test]
fn search_alone_emits_or_chain_with_one_param() {
    let q = SelectQuery {
        search: Some(SearchClause {
            columns: vec!["name", "is_active"],
            query: "ali".into(),
        }),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE ("name" ILIKE $1 OR "is_active" ILIKE $1)"#,
    );
    assert_eq!(stmt.params, vec![SqlValue::String("%ali%".into())]);
}

#[test]
fn search_combined_with_filter_uses_and() {
    let q = SelectQuery {
        where_clause: WhereExpr::Predicate(Filter {
            column: "is_active",
            op: Op::Eq,
            value: SqlValue::Bool(true),
        }),
        search: Some(SearchClause {
            columns: vec!["name"],
            query: "ali".into(),
        }),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE "is_active" = $1 AND ("name" ILIKE $2)"#,
    );
    assert_eq!(
        stmt.params,
        vec![SqlValue::Bool(true), SqlValue::String("%ali%".into())],
    );
}

#[test]
fn empty_search_query_emits_no_clause() {
    let q = SelectQuery {
        search: Some(SearchClause {
            columns: vec!["name"],
            query: String::new(),
        }),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(stmt.sql, r#"SELECT "id", "name", "is_active" FROM "user""#);
    assert!(stmt.params.is_empty());
}

#[test]
fn empty_search_columns_emits_no_clause() {
    let q = SelectQuery {
        search: Some(SearchClause {
            columns: vec![],
            query: "anything".into(),
        }),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(stmt.sql, r#"SELECT "id", "name", "is_active" FROM "user""#);
}

#[test]
fn search_with_limit_offset_orders_clauses_correctly() {
    let q = SelectQuery {
        search: Some(SearchClause {
            columns: vec!["name"],
            query: "x".into(),
        }),
        limit: Some(10),
        offset: Some(20),
        ..empty_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "id", "name", "is_active" FROM "user" WHERE ("name" ILIKE $1) LIMIT 10 OFFSET 20"#,
    );
}

// ---------------- LEFT JOIN ----------------

fn empty_post_select() -> SelectQuery {
    SelectQuery {
        model: Post::SCHEMA,
        where_clause: WhereExpr::And(vec![]),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
    }
}

#[test]
fn join_qualifies_main_columns_and_aliases_joined_ones() {
    let q = SelectQuery {
        joins: vec![Join {
            target: User::SCHEMA,
            on_local: "author_id",
            on_remote: "id",
            alias: "author_id",
            project: vec!["name"],
        }],
        ..empty_post_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(
        stmt.sql,
        r#"SELECT "post"."id", "post"."title", "post"."author_id", "author_id"."name" AS "author_id__name" FROM "post" LEFT JOIN "user" AS "author_id" ON "post"."author_id" = "author_id"."id""#,
    );
    assert!(stmt.params.is_empty());
}

#[test]
fn join_with_filter_qualifies_filter_column() {
    let q = SelectQuery {
        where_clause: WhereExpr::Predicate(Filter {
            column: "title",
            op: Op::Eq,
            value: SqlValue::String("hi".into()),
        }),
        joins: vec![Join {
            target: User::SCHEMA,
            on_local: "author_id",
            on_remote: "id",
            alias: "author_id",
            project: vec!["name"],
        }],
        ..empty_post_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "post"."title" = $1"#),
        "filter column not qualified: {}",
        stmt.sql,
    );
    assert!(stmt.sql.contains(r#"LEFT JOIN "user" AS "author_id""#));
}

#[test]
fn join_with_search_qualifies_search_columns() {
    let q = SelectQuery {
        search: Some(SearchClause {
            columns: vec!["title"],
            query: "hi".into(),
        }),
        joins: vec![Join {
            target: User::SCHEMA,
            on_local: "author_id",
            on_remote: "id",
            alias: "author_id",
            project: vec!["name"],
        }],
        ..empty_post_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE ("post"."title" ILIKE $1)"#),
        "search column not qualified: {}",
        stmt.sql,
    );
}

#[test]
fn no_joins_keeps_unqualified_select_shape() {
    // Backwards compatibility: when `joins` is empty, the SELECT must match
    // the existing unqualified shape so existing tests + admin output don't
    // shift around.
    let q = empty_post_select();
    let stmt = pg().compile_select(&q).unwrap();
    assert_eq!(stmt.sql, r#"SELECT "id", "title", "author_id" FROM "post""#,);
}

#[test]
fn join_with_limit_and_offset_orders_clauses() {
    let q = SelectQuery {
        joins: vec![Join {
            target: User::SCHEMA,
            on_local: "author_id",
            on_remote: "id",
            alias: "author_id",
            project: vec!["name"],
        }],
        limit: Some(10),
        offset: Some(20),
        ..empty_post_select()
    };
    let stmt = pg().compile_select(&q).unwrap();
    // LIMIT/OFFSET come after the WHERE-less LEFT JOIN.
    assert!(
        stmt.sql.ends_with("LIMIT 10 OFFSET 20"),
        "tail wrong: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains("LEFT JOIN"));
}
