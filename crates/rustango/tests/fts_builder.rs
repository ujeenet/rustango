//! Full-text-search builder — closes #295 / T2.4.
//!
//! Pins the PG emission shape for `SearchVector` / `SearchQuery` /
//! `SearchRank` / `SearchHeadline` and verifies MySQL + SQLite reject
//! with `OpNotSupportedInDialect`. FTS is **PG-only by language
//! semantics**, so non-PG dialects MUST surface a clear error rather
//! than silently fall back to ILIKE.

use rustango::core::fts::{SearchHeadline, SearchQuery, SearchRank, SearchVector, Weight};
use rustango::core::{Expr, F};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};

fn pg(e: &Expr) -> Result<String, SqlError> {
    write_for_test(&Postgres, e)
}

fn my(e: &Expr) -> Result<String, SqlError> {
    write_for_test(&MySql, e)
}

fn sqlite(e: &Expr) -> Result<String, SqlError> {
    write_for_test(&Sqlite, e)
}

fn write_for_test(dialect: &dyn Dialect, e: &Expr) -> Result<String, SqlError> {
    use rustango::core::{Op, SqlValue, WhereExpr};
    let qs = rustango::query::QuerySet::<NoModel>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e.clone(),
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::Bool(true)),
    });
    let select = qs.compile().unwrap();
    Ok(dialect.compile_select(&select)?.sql)
}

fn write_where_for_test(
    dialect: &dyn Dialect,
    w: rustango::core::WhereExpr,
) -> Result<String, SqlError> {
    let qs = rustango::query::QuerySet::<NoModel>::default().where_raw(w);
    let select = qs.compile().unwrap();
    Ok(dialect.compile_select(&select)?.sql)
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "fts_demo")]
#[allow(dead_code)]
pub struct NoModel {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    body: String,
}

// ---------- SearchVector ----------

#[test]
fn search_vector_single_emits_to_tsvector_call() {
    let v = SearchVector::single(F("title"));
    let pg = pg(v.as_expr()).unwrap();
    assert!(pg.contains(r#"to_tsvector("title")"#), "PG: {pg}");
}

#[test]
fn search_vector_weighted_emits_setweight_chain_on_pg() {
    let v = SearchVector::weighted([(F("title"), Weight::A), (F("body"), Weight::B)]);
    let pg = pg(v.as_expr()).unwrap();
    assert!(
        pg.contains(r#"setweight(to_tsvector("title"), 'A')"#),
        "PG setweight A: {pg}"
    );
    assert!(
        pg.contains(r#"setweight(to_tsvector("body"), 'B')"#),
        "PG setweight B: {pg}"
    );
    // The two clauses must be joined by `||`.
    assert!(pg.contains(" || "), "PG `||` concat: {pg}");
}

#[test]
fn search_vector_weighted_single_pair_omits_concat_wrapper() {
    // Single-column weighted: just one setweight, no `||` chain.
    let v = SearchVector::weighted([(F("title"), Weight::A)]);
    let pg = pg(v.as_expr()).unwrap();
    assert!(
        pg.contains(r#"setweight(to_tsvector("title"), 'A')"#),
        "PG: {pg}"
    );
    assert!(!pg.contains(" || "), "single-pair must not chain: {pg}");
}

#[test]
fn search_vector_weighted_errors_on_non_pg() {
    let v = SearchVector::weighted([(F("title"), Weight::A), (F("body"), Weight::B)]);
    for err in [
        my(v.as_expr()).unwrap_err(),
        sqlite(v.as_expr()).unwrap_err(),
    ] {
        assert!(matches!(err, SqlError::OpNotSupportedInDialect { .. }));
    }
}

// ---------- SearchQuery ----------

#[test]
fn search_query_plain_emits_plainto_tsquery() {
    let q = SearchQuery::plain("alice");
    let pg = pg(q.as_expr()).unwrap();
    assert!(pg.contains("plainto_tsquery("), "PG plain: {pg}");
}

#[test]
fn search_query_phrase_emits_phraseto_tsquery() {
    let q = SearchQuery::phrase("hello world");
    let pg = pg(q.as_expr()).unwrap();
    assert!(pg.contains("phraseto_tsquery("), "PG phrase: {pg}");
}

#[test]
fn search_query_websearch_emits_websearch_to_tsquery() {
    let q = SearchQuery::websearch(r#""rust orm" -django"#);
    let pg = pg(q.as_expr()).unwrap();
    assert!(pg.contains("websearch_to_tsquery("), "PG websearch: {pg}");
}

#[test]
fn search_query_raw_emits_to_tsquery() {
    let q = SearchQuery::raw("rust & !python");
    let pg = pg(q.as_expr()).unwrap();
    assert!(pg.contains("to_tsquery("), "PG raw: {pg}");
}

// ---------- matches() → WhereExpr ----------

#[test]
fn matches_emits_tsvector_at_at_tsquery_on_pg() {
    let v = SearchVector::single(F("title"));
    let q = SearchQuery::plain("alice");
    let pg = write_where_for_test(&Postgres, v.matches(&q)).unwrap();
    assert!(
        pg.contains(r#"to_tsvector("title") @@ plainto_tsquery("#),
        "PG @@ match: {pg}"
    );
}

#[test]
fn matches_errors_on_non_pg() {
    let v = SearchVector::single(F("title"));
    let q = SearchQuery::plain("alice");
    for err in [
        write_where_for_test(&MySql, v.matches(&q)).unwrap_err(),
        write_where_for_test(&Sqlite, v.matches(&q)).unwrap_err(),
    ] {
        assert!(matches!(err, SqlError::OpNotSupportedInDialect { .. }));
    }
}

// ---------- SearchRank ----------

#[test]
fn search_rank_emits_ts_rank_with_vector_and_query() {
    let v = SearchVector::single(F("title"));
    let q = SearchQuery::plain("alice");
    let e = SearchRank::new(&v, &q);
    let pg = pg(&e).unwrap();
    assert!(
        pg.contains(r#"ts_rank(to_tsvector("title"), plainto_tsquery("#),
        "PG ts_rank: {pg}"
    );
}

#[test]
fn search_rank_cover_density_emits_ts_rank_cd() {
    let v = SearchVector::single(F("title"));
    let q = SearchQuery::plain("alice");
    let e = SearchRank::cover_density(&v, &q);
    let pg = pg(&e).unwrap();
    assert!(pg.contains("ts_rank_cd("), "PG ts_rank_cd: {pg}");
}

// ---------- SearchHeadline ----------

#[test]
fn search_headline_emits_ts_headline_two_arg() {
    let q = SearchQuery::plain("alice");
    let e = SearchHeadline::new(F("body"), &q);
    let pg = pg(&e).unwrap();
    assert!(
        pg.contains(r#"ts_headline("body", plainto_tsquery("#),
        "PG ts_headline: {pg}"
    );
}

#[test]
fn search_headline_with_options_emits_three_arg_form() {
    let q = SearchQuery::plain("alice");
    let e = SearchHeadline::with_options(
        F("body"),
        &q,
        "StartSel='<mark>', StopSel='</mark>', MaxFragments=1",
    );
    let pg = pg(&e).unwrap();
    assert!(pg.contains("ts_headline("), "PG ts_headline: {pg}");
    // Options string lands as a bound parameter ($N), not inline.
    assert!(
        pg.contains("$"),
        "options arg should bind as a parameter: {pg}"
    );
}

// ---------- Weighted multi-column composed with ts_rank ----------

#[test]
fn full_composition_weighted_vector_with_ts_rank() {
    let v = SearchVector::weighted([(F("title"), Weight::A), (F("body"), Weight::B)]);
    let q = SearchQuery::plain("alice");
    let pg_rank = pg(&SearchRank::new(&v, &q)).unwrap();
    // The weighted vector must appear unchanged inside ts_rank.
    assert!(
        pg_rank.contains("ts_rank(") && pg_rank.contains("setweight(to_tsvector"),
        "PG ts_rank with weighted vector: {pg_rank}"
    );
}
