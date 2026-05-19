//! `Q()` runtime composable predicate — closes #263 / T1.1.
//!
//! Each test pins:
//!   1. The per-lookup constructor lowers to the expected `WhereExpr`.
//!   2. Operator overloads (`&` / `|` / `^` / `!`) produce the right
//!      tree shape and compile to correct SQL on all three dialects.
//!   3. The wrapped `Q` works in both `.where_raw(q.into())` (untyped)
//!      and `.where_(q)` (typed via Into<TypedExpr<T>>).

use rustango::core::Model as _;
use rustango::query::{QuerySet, Q};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "q_builder_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    email: String,
    age: i64,
    banned: bool,
}

fn compile_pg(q: Q) -> String {
    let qs: QuerySet<User> = QuerySet::default().where_(q);
    let s = qs.compile().unwrap();
    Postgres.compile_select(&s).unwrap().sql
}

fn compile_mysql(q: Q) -> String {
    let qs: QuerySet<User> = QuerySet::default().where_(q);
    let s = qs.compile().unwrap();
    MySql.compile_select(&s).unwrap().sql
}

fn compile_sqlite(q: Q) -> String {
    let qs: QuerySet<User> = QuerySet::default().where_(q);
    let s = qs.compile().unwrap();
    Sqlite.compile_select(&s).unwrap().sql
}

// ---------- Per-lookup constructors ----------

#[test]
fn eq_produces_eq_clause() {
    let sql = compile_pg(Q::eq("id", 1_i64));
    assert!(sql.contains(r#""id" = $1"#), "got: {sql}");
}

#[test]
fn ne_produces_ne_clause() {
    let sql = compile_pg(Q::ne("id", 1_i64));
    assert!(sql.contains(r#""id" <> $1"#), "got: {sql}");
}

#[test]
fn comparison_constructors() {
    assert!(compile_pg(Q::gt("age", 18_i64)).contains(r#""age" > $1"#));
    assert!(compile_pg(Q::gte("age", 18_i64)).contains(r#""age" >= $1"#));
    assert!(compile_pg(Q::lt("age", 65_i64)).contains(r#""age" < $1"#));
    assert!(compile_pg(Q::lte("age", 65_i64)).contains(r#""age" <= $1"#));
}

#[test]
fn icontains_wraps_with_percent_and_uses_ilike() {
    let sql = compile_pg(Q::icontains("email", "alice"));
    assert!(sql.contains("ILIKE"), "expected ILIKE in: {sql}");
}

#[test]
fn startswith_emits_like_with_trailing_wildcard() {
    let sql = compile_pg(Q::startswith("email", "ali"));
    assert!(sql.contains(r#""email" LIKE $1"#), "got: {sql}");
}

#[test]
fn in_takes_iterable() {
    let sql = compile_pg(Q::in_("id", vec![1_i64, 2, 3]));
    assert!(sql.contains(r#""id" IN ($1, $2, $3)"#), "got: {sql}");
}

#[test]
fn not_in_takes_iterable() {
    let sql = compile_pg(Q::not_in("id", vec![1_i64, 2, 3]));
    assert!(sql.contains(r#""id" NOT IN ($1, $2, $3)"#), "got: {sql}");
}

#[test]
fn is_null_routes_through_op_is_null() {
    let sql = compile_pg(Q::is_null("email"));
    assert!(sql.contains(r#""email" IS NULL"#), "got: {sql}");
}

#[test]
fn is_not_null_routes_correctly() {
    let sql = compile_pg(Q::is_not_null("email"));
    assert!(sql.contains(r#""email" IS NOT NULL"#), "got: {sql}");
}

#[test]
fn between_emits_between_lo_and_hi() {
    let sql = compile_pg(Q::between("age", 18_i64, 65_i64));
    assert!(sql.contains("BETWEEN"), "expected BETWEEN in: {sql}");
}

// ---------- Operator overloads ----------

#[test]
fn bitand_emits_and() {
    let q = Q::eq("id", 1_i64) & Q::eq("banned", false);
    let sql = compile_pg(q);
    assert!(sql.contains(" AND "), "got: {sql}");
}

#[test]
fn bitor_emits_or() {
    let q = Q::eq("id", 1_i64) | Q::eq("id", 2_i64);
    let sql = compile_pg(q);
    assert!(sql.contains(" OR "), "got: {sql}");
}

#[test]
fn unary_not_emits_not() {
    let q = !Q::eq("banned", true);
    let sql = compile_pg(q);
    assert!(sql.contains("NOT"), "got: {sql}");
}

#[test]
fn complex_django_shape_lowers_correctly() {
    // Django: Q(email__startswith='A') | (Q(age__gt=18) & ~Q(banned=True))
    let q = Q::startswith("email", "A") | (Q::gt("age", 18_i64) & !Q::eq("banned", true));
    let sql = compile_pg(q);
    assert!(sql.contains(" OR "), "expected OR: {sql}");
    assert!(sql.contains(" AND "), "expected AND: {sql}");
    assert!(sql.contains("NOT"), "expected NOT: {sql}");
    assert!(sql.contains("LIKE"), "expected startswith LIKE: {sql}");
}

// ---------- Tri-dialect parity ----------

#[test]
fn same_q_emits_correct_sql_on_all_three_backends() {
    let q = Q::icontains("email", "alice") & !Q::eq("banned", true);
    let pg = compile_pg(q.clone());
    let my = compile_mysql(q.clone());
    let lite = compile_sqlite(q);
    assert!(pg.contains("ILIKE"), "PG: {pg}");
    // MySQL ILIKE → LOWER(...) LIKE LOWER(...) (case-insensitive fallback).
    assert!(my.contains("LOWER"), "MySQL: {my}");
    // SQLite — case-insensitive LIKE via LOWER too.
    assert!(lite.contains("LIKE"), "SQLite: {lite}");
}

// ---------- Method-style aliases ----------

#[test]
fn method_chain_aliases_match_operator_form() {
    let op_form = Q::eq("id", 1_i64) | Q::eq("id", 2_i64);
    let method_form = Q::eq("id", 1_i64).or(Q::eq("id", 2_i64));
    assert_eq!(compile_pg(op_form), compile_pg(method_form));
}

#[test]
fn negate_method_matches_unary_not() {
    let op_form = !Q::eq("banned", true);
    let method_form = Q::eq("banned", true).negate();
    assert_eq!(compile_pg(op_form), compile_pg(method_form));
}

// ---------- Integration with where_/where_raw ----------

#[test]
fn q_works_via_where_raw_path() {
    let q = Q::eq("id", 1_i64);
    let qs: QuerySet<User> = QuerySet::default().where_raw(q.into());
    let sql = Postgres.compile_select(&qs.compile().unwrap()).unwrap().sql;
    assert!(sql.contains(r#""id" = $1"#), "got: {sql}");
}

#[test]
fn unknown_field_in_q_does_not_panic_at_compile() {
    // Field-name validation for `WhereExpr::Predicate` is the same
    // pre-existing gap that `where_raw()` carries: the typed
    // `User::field.eq()` path catches typos at compile *time* because
    // the const doesn't exist, but the runtime `Q::eq("name", ...)`
    // shape resolves at the database. That's the documented
    // trade-off for runtime composability — for compile-time safety
    // on statically-known field names, reach for `Q!()` (issue #269)
    // or the typed `User::field.eq()` path.
    //
    // This test pins that the queryset still *compiles to SQL*
    // without panicking — the DB would reject at exec.
    let q = Q::eq("no_such_field", 1_i64);
    let qs: QuerySet<User> = QuerySet::default().where_raw(q.into());
    let select = qs
        .compile()
        .expect("compile should not panic on unknown field");
    let sql = Postgres.compile_select(&select).unwrap().sql;
    // Unknown field is rendered verbatim; the DB layer rejects it.
    assert!(sql.contains(r#""no_such_field""#));
}
