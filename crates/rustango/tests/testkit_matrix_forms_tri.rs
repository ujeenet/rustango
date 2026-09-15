//! The harness's own forms, exercised through the macro (#1461).
//!
//! `explain_pool_tri` and `bulk_upsert_tri` both use the common shape —
//! `model:` plus in-memory SQLite. This file covers the other two, which
//! exist because 29 of the 192 SQLite suites cannot use either:
//!
//! * **`setup:`** — the suite builds its own tables. The job queue calls
//!   `ensure_table_pool`; a migration suite runs migrations. There is no
//!   single model whose schema describes the state they need.
//! * **`sqlite: file`** — `sqlite::memory:` is per-*connection*, so two
//!   connections to it are two separate databases. Any suite whose
//!   workers must see each other's rows tests nothing against it.
//!
//! Without a test at this level those two forms are only proven by the
//! suites that will later depend on them, which is backwards.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{Auto, CounterPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "matrix_forms_row")]
#[rustango(app = "testkit_matrix_forms_tri")]
#[allow(dead_code)]
pub struct Row {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub label: String,
}

/// A setup that is not `fresh_table::<M>`.
///
/// Deliberately does something a model cannot express on its own — it
/// builds the table *and* seeds it — so a scenario asserting on the seed
/// proves this ran rather than some default path.
async fn build_and_seed(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Row>(pool).await;

    let mut seeded = Row {
        id: Auto::Unset,
        label: "from-setup".into(),
    };
    seeded
        .save_pool(pool)
        .await
        .expect("seed row written by the custom setup");
}

/// The custom setup ran, on whichever backend this is.
///
/// If `setup:` were ignored the table would not exist and this would
/// error rather than return zero — the two failure modes are worth
/// distinguishing, which is why the assertion is on the seeded row and
/// not merely on the count.
async fn the_custom_setup_ran(pool: &Pool) {
    use rustango::sql::FetcherPool as _;

    let rows: Vec<Row> = Row::objects().fetch(pool).await.expect("fetch");
    assert_eq!(
        rows.len(),
        1,
        "`setup:` should have created and seeded the table"
    );
    assert_eq!(
        rows[0].label, "from-setup",
        "the row must be the one the custom setup wrote"
    );
}

/// Each test gets a fresh database, so the seed is exactly one row —
/// never two from a previous scenario in the same file.
async fn setup_runs_per_scenario_not_once(pool: &Pool) {
    assert_eq!(
        Row::objects().count(pool).await.expect("count"),
        1,
        "a second scenario must start from its own setup, not inherit the first's \
         rows — a shared database across scenarios is how order-dependent suites \
         start passing for the wrong reason"
    );
}

tri_dialect_test! {
    setup: build_and_seed,
    sqlite: file,
    scenarios: [
        the_custom_setup_ran,
        setup_runs_per_scenario_not_once,
    ],
}
