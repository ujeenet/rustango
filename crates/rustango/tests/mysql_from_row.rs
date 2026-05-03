//! Compile-time check that `#[derive(Model)]` emits an
//! `impl FromRow<MySqlRow>` when rustango is built with the `mysql`
//! feature. The check is the type assertion alone — if the proc-macro
//! → `__impl_my_from_row!` → `impl<'r> FromRow<'r, MySqlRow>` chain
//! breaks, this test fails to compile.
//!
//! Skipped under PG-only builds via `#![cfg(feature = "mysql")]`. The
//! existing `tests/derive_model.rs` covers the `FromRow<PgRow>` side
//! end-to-end.

#![cfg(feature = "mysql")]

use rustango::Model;

#[derive(Model)]
#[rustango(table = "mysql_from_row_users")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    name: String,
    email: Option<String>,
    is_active: bool,
}

#[derive(Model)]
#[rustango(table = "mysql_from_row_posts")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    title: String,
    body: String,
}

fn assert_my_from_row<T>()
where
    T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
{
}

#[test]
fn user_model_implements_my_from_row() {
    assert_my_from_row::<User>();
    assert_my_from_row::<Post>();
}

#[test]
fn user_model_also_implements_pg_from_row() {
    // Regression guard: the macro must still emit the PG impl
    // alongside the MySQL one — the `__impl_my_from_row!` call sits
    // *after* the existing `impl FromRow<PgRow>`, but a refactor
    // that accidentally replaces (vs. adds) would silently break PG.
    fn assert_pg_from_row<T>()
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
    {
    }
    assert_pg_from_row::<User>();
    assert_pg_from_row::<Post>();
}

#[test]
fn maybe_my_from_row_resolves_for_derived_model() {
    // The MaybeMyFromRow bound is what `select_rows_pool` and
    // `FetcherPool::fetch_pool` use. Confirm derived models satisfy
    // it under the mysql feature config.
    fn check<T: rustango::sql::MaybeMyFromRow>() {}
    check::<User>();
    check::<Post>();
}
