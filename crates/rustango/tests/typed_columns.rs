//! Typed columns: `User::id`, `User::name`, etc., with compile-time-checked
//! filter and SET expressions. Verifies the typed API produces the same
//! `SelectQuery`/`UpdateQuery` IR as the string-keyed API.

use rustango::core::{Column as _, FieldType, Op, SqlValue};
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    email: Option<String>,
    is_active: bool,
}

// ---------------- typed filters ----------------

#[test]
fn typed_eq_is_equivalent_to_string_eq() {
    let typed = User::objects()
        .where_(User::name.eq("alice"))
        .compile()
        .unwrap();
    let string_keyed = User::objects().eq("name", "alice").compile().unwrap();
    assert_eq!(typed.where_clause, string_keyed.where_clause);
}

#[test]
fn typed_ne_lt_lte_gt_gte() {
    let q = User::objects()
        .where_(User::id.ne(0_i64))
        .where_(User::id.lt(100_i64))
        .where_(User::id.lte(200_i64))
        .where_(User::id.gt(-1_i64))
        .where_(User::id.gte(-2_i64))
        .compile()
        .unwrap();
    let filters = q.where_clause.as_flat_and().unwrap();
    let ops: Vec<Op> = filters.iter().map(|f| f.op).collect();
    assert_eq!(ops, vec![Op::Ne, Op::Lt, Op::Lte, Op::Gt, Op::Gte]);
    assert!(filters.iter().all(|f| f.column == "id"));
}

#[test]
fn typed_like_on_string_field() {
    let q = User::objects()
        .where_(User::name.like("ali%"))
        .compile()
        .unwrap();
    let filters = q.where_clause.as_flat_and().unwrap();
    assert_eq!(filters[0].column, "name");
    assert_eq!(filters[0].op, Op::Like);
    assert_eq!(filters[0].value, SqlValue::String("ali%".into()));
}

#[test]
fn typed_is_null_and_is_not_null() {
    let q = User::objects()
        .where_(User::email.is_null())
        .where_(User::email.is_not_null())
        .compile()
        .unwrap();
    let filters = q.where_clause.as_flat_and().unwrap();
    assert_eq!(filters[0].op, Op::IsNull);
    assert_eq!(filters[0].value, SqlValue::Bool(true));
    assert_eq!(filters[1].op, Op::IsNull);
    assert_eq!(filters[1].value, SqlValue::Bool(false));
}

#[test]
fn typed_is_in_expands_to_list() {
    let q = User::objects()
        .where_(User::id.is_in([1_i64, 2, 3]))
        .compile()
        .unwrap();
    let filters = q.where_clause.as_flat_and().unwrap();
    assert_eq!(filters[0].column, "id");
    assert_eq!(filters[0].op, Op::In);
    assert_eq!(
        filters[0].value,
        SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)]),
    );
}

#[test]
fn typed_filters_can_be_mixed_with_string_keyed_in_order() {
    let q = User::objects()
        .eq("name", "alice")
        .where_(User::is_active.eq(true))
        .filter_op("id", Op::Gt, 0_i64)
        .compile()
        .unwrap();
    let filters = q.where_clause.as_flat_and().unwrap();
    assert_eq!(filters.len(), 3);
    assert_eq!(filters[0].column, "name");
    assert_eq!(filters[1].column, "is_active");
    assert_eq!(filters[2].column, "id");
}

#[test]
fn typed_filters_compile_to_same_postgres_sql() {
    let typed = User::objects()
        .where_(User::name.eq("alice"))
        .where_(User::is_active.eq(true))
        .compile()
        .unwrap();
    let stmt_typed = Postgres.compile_select(&typed).unwrap();

    let string_keyed = User::objects()
        .eq("name", "alice")
        .filter_op("is_active", Op::Eq, true)
        .compile()
        .unwrap();
    let stmt_string = Postgres.compile_select(&string_keyed).unwrap();

    assert_eq!(stmt_typed.sql, stmt_string.sql);
    assert_eq!(stmt_typed.params, stmt_string.params);
}

// ---------------- typed SET on UpdateBuilder ----------------

#[test]
fn typed_set_is_equivalent_to_string_set() {
    let typed = User::objects()
        .where_(User::id.eq(7_i64))
        .update()
        .set_typed(User::name.set("ALICE"))
        .set_typed(User::is_active.set(false))
        .compile()
        .unwrap();
    let string_keyed = User::objects()
        .eq("id", 7_i64)
        .update()
        .set("name", "ALICE")
        .set("is_active", false)
        .compile()
        .unwrap();
    assert_eq!(typed.set, string_keyed.set);
    assert_eq!(typed.where_clause, string_keyed.where_clause);
}

#[test]
fn typed_and_string_set_can_be_mixed() {
    let q = User::objects()
        .update()
        .set_typed(User::name.set("X"))
        .set("is_active", true)
        .compile()
        .unwrap();
    assert_eq!(q.set.len(), 2);
    assert_eq!(q.set[0].column, "name");
    assert_eq!(q.set[1].column, "is_active");
}

// ---------------- column-trait surface ----------------

#[test]
fn column_field_type_is_correct() {
    use rustango::core::Column as ColumnTrait;
    fn check<C: ColumnTrait>(_c: C, expected: FieldType) {
        assert_eq!(C::FIELD_TYPE, expected);
    }
    check(User::id, FieldType::I64);
    check(User::name, FieldType::String);
    // Option<String> resolves to FieldType::String at the kind level.
    check(User::email, FieldType::String);
    check(User::is_active, FieldType::Bool);
}

#[test]
fn column_name_and_column_match_the_field() {
    use rustango::core::Column as ColumnTrait;
    fn check<C: ColumnTrait>(_c: C, name: &str, column: &str) {
        assert_eq!(C::NAME, name);
        assert_eq!(C::COLUMN, column);
    }
    check(User::id, "id", "id");
    check(User::name, "name", "name");
    check(User::email, "email", "email");
}
