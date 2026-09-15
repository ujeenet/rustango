//! The harness's own forms, exercised through the macro (#1461).
//!
//! `explain_pool_tri` and `bulk_upsert_tri` both use the common shape —
//! `model:` plus in-memory SQLite. This file covers the other two, which
//! exist because 29 of the 192 SQLite suites cannot use either:
//!
//! * **`setup:`** — the suite builds its own tables. The job queue calls
//!   `ensure_table_pool`; a migration suite runs migrations. There is no
//!   single model whose schema describes the state they need.
//! * **`sqlite: file`** — for WAL journalling, file locking, or a second
//!   *process*. **Not** for cross-connection visibility: that was the
//!   stated reason those 29 suites reach for a temp file, and it is
//!   false. sqlx shares an in-memory database across a pool's
//!   connections, measured with a barrier forcing eight simultaneous
//!   ones (`sqlite_file_pool`'s doc carries the measurement). This
//!   header asserted the per-connection premise for three commits after
//!   the file that defines the form had already recorded its refutation
//!   — which is how a premise survives being disproved.
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
/// Each of the two scenarios below writes a row and then requires the
/// table to hold exactly two: the seed, plus its own.
///
/// The writing is what makes the assertion capable of failing. An
/// earlier version asserted only `count == 1` and wrote nothing, so a
/// setup hoisted to run once for the whole suite satisfied it in every
/// scenario — the test named the property and then checked something
/// that holds either way.
///
/// With per-scenario setup both see 2. With one shared setup, whichever
/// runs second sees 3.
async fn insert_and_expect_only_our_own(pool: &Pool, label: &str) {
    let mut mine = Row {
        id: Auto::Unset,
        label: label.into(),
    };
    mine.save_pool(pool).await.expect("write our own row");

    assert_eq!(
        Row::objects().count(pool).await.expect("count"),
        2,
        "expected the seed plus this scenario's own row. More means the setup ran \
         once for the whole suite and scenarios are sharing a database, which is \
         how order-dependent suites start passing for the wrong reason"
    );
}

async fn setup_runs_per_scenario_not_once(pool: &Pool) {
    insert_and_expect_only_our_own(pool, "from-scenario-a").await;
}

async fn and_again_for_the_following_scenario(pool: &Pool) {
    insert_and_expect_only_our_own(pool, "from-scenario-b").await;
}

tri_dialect_test! {
    setup: build_and_seed,
    sqlite: file,
    scenarios: [
        the_custom_setup_ran,
        setup_runs_per_scenario_not_once,
        and_again_for_the_following_scenario,
    ],
}
