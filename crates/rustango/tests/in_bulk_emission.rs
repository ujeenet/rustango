//! Tri-dialect SQL-emission sanity tests for the WHERE clause built
//! by `QuerySet::in_bulk_pool` (issue #24). The map-building part runs
//! against a live DB in `in_bulk_live.rs`; this file pins the SQL
//! shape (a plain `WHERE <col> IN (...)`) across all three dialects
//! without needing a database.

use rustango::core::{Model as _, Op, SqlValue};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "ibulk_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32, unique)]
    isbn: String,
    #[rustango(max_length = 64)]
    title: String,
}

// `in_bulk_pool` is `async` + needs a Pool, so we can't call it from
// a sync unit test without mocking. The function's only SQL-affecting
// step is `self.filter_op(C::COLUMN, Op::In, SqlValue::List(ids))`
// — pin that emission shape directly here.

fn build_in_filter<C: rustango::core::Column<Model = Book>>(
    _column: C,
    ids: Vec<SqlValue>,
) -> rustango::core::SelectQuery {
    Book::objects()
        .filter_op(C::COLUMN, Op::In, SqlValue::List(ids))
        .compile()
        .unwrap()
}

#[test]
fn in_bulk_emits_where_in_on_pg() {
    let q = build_in_filter(
        Book::id,
        vec![SqlValue::I64(1), SqlValue::I64(2), SqlValue::I64(3)],
    );
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "id" IN ($1, $2, $3)"#),
        "PG: WHERE id IN list: {}",
        stmt.sql
    );
    assert_eq!(stmt.params.len(), 3);
}

#[cfg(feature = "mysql")]
#[test]
fn in_bulk_emits_where_in_on_mysql() {
    let q = build_in_filter(Book::id, vec![SqlValue::I64(1), SqlValue::I64(2)]);
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains("WHERE `id` IN (?, ?)"),
        "MySQL: WHERE id IN list (backticks): {}",
        stmt.sql
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn in_bulk_emits_where_in_on_sqlite() {
    let q = build_in_filter(
        Book::isbn,
        vec![
            SqlValue::String("isbn-1".into()),
            SqlValue::String("isbn-2".into()),
        ],
    );
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "isbn" IN (?, ?)"#),
        "SQLite: WHERE isbn IN list: {}",
        stmt.sql
    );
}

/// Empty ids → short-circuit returns empty map, no SQL. The runtime
/// guards against this so `Op::In` with `SqlValue::List(vec![])` (which
/// the writer rejects with `EmptyInList`) never reaches the compiler.
/// Verify via a direct construction that an empty IN list IS rejected
/// at compile — proves the short-circuit is necessary, not vestigial.
#[test]
fn empty_in_list_rejects_at_compile_so_short_circuit_is_load_bearing() {
    use rustango::sql::SqlError;
    let q = Book::objects()
        .filter_op("id", Op::In, SqlValue::List(vec![]))
        .compile()
        .unwrap();
    let err = Postgres.compile_select(&q).unwrap_err();
    assert!(
        matches!(err, SqlError::EmptyInList),
        "raw empty IN list rejected: {err:?}"
    );
    // `in_bulk_pool` with empty ids never reaches this codepath — it
    // returns an empty HashMap before constructing the IN list. See
    // `in_bulk_with_empty_ids_returns_empty_map_no_sql` in
    // `tests/in_bulk_live.rs` for the runtime guarantee.
}
