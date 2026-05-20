#![cfg(all(feature = "sqlite", not(feature = "postgres")))]
//! Regression: `#[derive(Model)]` on a model with a `ForeignKey<T>`
//! field must compile under non-PG feature sets.
//!
//! Pre-fix: the reverse-FK accessor (`parent.{child}_set(executor)`)
//! was emitted unconditionally, with a `Database = sqlx::Postgres`
//! executor bound + a `.fetch_on(...)` call — both gated behind the
//! `postgres` cargo feature. Under `--no-default-features --features
//! sqlite,tenancy` the macro expansion failed to compile.
//!
//! Post-fix: the reverse-FK accessor is wrapped in
//! `#[cfg(feature = "postgres")]` inside the macro, so non-PG builds
//! see the model derive but not the accessor. This file pins that
//! invariant — if the cfg gate ever regresses, this file fails to
//! compile under `cargo build -p rustango --no-default-features
//! --features sqlite,tenancy --test macro_fk_no_postgres`, which the
//! `sqlite_litmus` CI job runs.

use rustango::sql::{Auto, ForeignKey};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfknp_parent")]
#[allow(dead_code)]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfknp_child")]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The presence of this field used to trigger PG-only emission
    /// in the macro's `reverse_helper_tokens` — `impl Parent { pub
    /// async fn child_set(&self, _: impl Executor<Database =
    /// Postgres>) -> Vec<Child> }`. Now gated behind
    /// `#[cfg(feature = "postgres")]` at emit time.
    pub parent: ForeignKey<Parent>,
}

#[test]
fn derives_compile_with_fk_under_sqlite_only() {
    use rustango::core::Model as _;
    assert_eq!(Parent::SCHEMA.table, "mfknp_parent");
    assert_eq!(Child::SCHEMA.table, "mfknp_child");
    // The reverse accessor `Parent::child_set` must NOT exist under
    // sqlite-only — confirming it would require either method-call
    // probing (not stable in Rust) or a separate negative test. The
    // compile-time success of this file is the load-bearing assertion.
}
