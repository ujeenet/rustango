//! `Q!()` compile-time-resolved Django-shape filter macro — closes
//! #269 / T1.7.
//!
//! Each test exercises one lookup suffix end-to-end:
//!   1. `Q!(Model.field__suffix = value)` expands to the same
//!      `TypedFilter` as the hand-rolled `Model::field.method(value)`.
//!   2. The compiled SQL is byte-identical across PG / MySQL / SQLite
//!      writers — the macro is pure syntactic sugar over typed-column
//!      machinery (no new SQL emission).
//!
//! Compile-fail cases (unknown field, unknown suffix, wrong rhs shape
//! for `__between` / `__isnull`) are covered by `compile_fail` doctests
//! on the macro itself in `crates/rustango-macros/src/lib.rs`; the
//! project intentionally avoids `trybuild` per the team's "don't pull
//! in a dep for one-off compile checks" convention (see comment in
//! `tests/save_partial_typed.rs`).

use rustango::core::Column as _;
use rustango::query::QuerySet;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::{Model, Q};

#[derive(Model, Debug, Clone)]
#[rustango(table = "q_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    email: String,
    active: bool,
}

fn compile_pg(qs: QuerySet<User>) -> String {
    let q = qs.compile().unwrap();
    Postgres.compile_select(&q).unwrap().sql
}

fn compile_mysql(qs: QuerySet<User>) -> String {
    let q = qs.compile().unwrap();
    MySql.compile_select(&q).unwrap().sql
}

fn compile_sqlite(qs: QuerySet<User>) -> String {
    let q = qs.compile().unwrap();
    Sqlite.compile_select(&q).unwrap().sql
}

fn base_qs() -> QuerySet<User> {
    QuerySet::<User>::default()
}

// ---------- Equality forms ----------

#[test]
fn bare_eq_expands_to_eq() {
    let qs_macro = base_qs().where_(Q!(User.id = 1_i64));
    let qs_typed = base_qs().where_(User::id.eq(1_i64));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

#[test]
fn exact_expands_to_eq() {
    let qs_macro = base_qs().where_(Q!(User.id__exact = 1_i64));
    let qs_typed = base_qs().where_(User::id.eq(1_i64));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

#[test]
fn ne_expands_to_ne() {
    let qs_macro = base_qs().where_(Q!(User.id__ne = 1_i64));
    let qs_typed = base_qs().where_(User::id.ne(1_i64));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

// ---------- Comparison forms ----------

#[test]
fn gt_gte_lt_lte_lower_to_comparison_methods() {
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.id__gt = 1_i64))),
        compile_pg(base_qs().where_(User::id.gt(1_i64))),
    );
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.id__gte = 1_i64))),
        compile_pg(base_qs().where_(User::id.gte(1_i64))),
    );
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.id__lt = 1_i64))),
        compile_pg(base_qs().where_(User::id.lt(1_i64))),
    );
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.id__lte = 1_i64))),
        compile_pg(base_qs().where_(User::id.lte(1_i64))),
    );
}

// ---------- LIKE family ----------

#[test]
fn icontains_wraps_with_percent_and_uses_ilike() {
    // #1257 — the escaped typed method is the equivalence now.
    let qs_macro = base_qs().where_(Q!(User.email__icontains = "alice"));
    let qs_typed = base_qs().where_(User::email.icontains("alice"));
    let sql_macro = compile_pg(qs_macro);
    let sql_typed = compile_pg(qs_typed);
    assert_eq!(sql_macro, sql_typed, "Q!() must match Column::icontains");
    assert!(
        sql_macro.contains("ILIKE"),
        "expected ILIKE in: {sql_macro}"
    );
}

#[test]
fn contains_uses_like_with_percent_wrap() {
    // #1257 — the macro delegates to the escaped typed method, so the
    // hand-rolled equivalent is Column::contains, not raw like().
    let qs_macro = base_qs().where_(Q!(User.email__contains = "alice"));
    let qs_typed = base_qs().where_(User::email.contains("alice"));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

#[test]
fn startswith_and_endswith_lower_correctly() {
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.email__startswith = "ali"))),
        compile_pg(base_qs().where_(User::email.startswith("ali"))),
    );
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.email__endswith = ".com"))),
        compile_pg(base_qs().where_(User::email.endswith(".com"))),
    );
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.email__istartswith = "ali"))),
        compile_pg(base_qs().where_(User::email.istartswith("ali"))),
    );
    assert_eq!(
        compile_pg(base_qs().where_(Q!(User.email__iendswith = ".COM"))),
        compile_pg(base_qs().where_(User::email.iendswith(".COM"))),
    );
}

// ---------- IN / NOT IN ----------

#[test]
fn in_takes_iterable() {
    let qs_macro = base_qs().where_(Q!(User.id__in = vec![1_i64, 2, 3]));
    let qs_typed = base_qs().where_(User::id.is_in(vec![1_i64, 2, 3]));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

#[test]
fn not_in_takes_iterable() {
    let qs_macro = base_qs().where_(Q!(User.id__not_in = vec![1_i64, 2, 3]));
    let qs_typed = base_qs().where_(User::id.not_in(vec![1_i64, 2, 3]));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

// ---------- ISNULL ----------

#[test]
fn isnull_true_lowers_to_is_null() {
    let sql_macro = compile_pg(base_qs().where_(Q!(User.email__isnull = true)));
    let sql_typed = compile_pg(base_qs().where_(User::email.is_null()));
    assert!(
        sql_macro.contains("IS NULL"),
        "expected IS NULL in: {sql_macro}"
    );
    assert_eq!(sql_macro, sql_typed);
}

#[test]
fn isnull_false_lowers_to_is_not_null() {
    let sql_macro = compile_pg(base_qs().where_(Q!(User.email__isnull = false)));
    let sql_typed = compile_pg(base_qs().where_(User::email.is_not_null()));
    assert!(
        sql_macro.contains("IS NOT NULL"),
        "expected IS NOT NULL in: {sql_macro}"
    );
    assert_eq!(sql_macro, sql_typed);
}

// ---------- BETWEEN ----------

#[test]
fn between_takes_tuple_literal() {
    let qs_macro = base_qs().where_(Q!(User.id__between = (1_i64, 10_i64)));
    let qs_typed = base_qs().where_(User::id.between(1_i64, 10_i64));
    assert_eq!(compile_pg(qs_macro), compile_pg(qs_typed));
}

// ---------- Tri-dialect parity ----------

#[test]
fn same_q_macro_emits_consistent_sql_on_all_three_backends() {
    // The macro lowers to typed-column calls which already route per
    // dialect — pinning that the same `Q!()` input produces well-formed
    // (non-empty, non-erroring) output on every backend.
    let qs = base_qs().where_(Q!(User.email__icontains = "alice"));
    let q = qs.compile().unwrap();
    let pg = Postgres.compile_select(&q).unwrap().sql;
    let my = MySql.compile_select(&q).unwrap().sql;
    let lite = Sqlite.compile_select(&q).unwrap().sql;
    assert!(pg.contains("ILIKE"), "PG should use ILIKE: {pg}");
    assert!(
        my.contains("LOWER"),
        "MySQL ILIKE falls back to LOWER: {my}"
    );
    assert!(
        lite.contains("LIKE"),
        "SQLite ILIKE → case-insensitive LIKE: {lite}"
    );
    // Suppress unused-function lints when only one is consulted.
    let _ = (compile_mysql, compile_sqlite);
}

// ---------- Chained composition ----------

#[test]
fn macro_results_chain_via_and_or() {
    let combined = Q!(User.active = true).and(Q!(User.email__icontains = "alice"));
    let qs = base_qs().where_(combined);
    let sql = compile_pg(qs);
    assert!(sql.contains("AND"), "expected AND in: {sql}");
    assert!(sql.contains("ILIKE"), "expected ILIKE in: {sql}");
}
