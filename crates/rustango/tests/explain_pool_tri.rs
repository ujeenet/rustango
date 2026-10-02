//! `sql::explain_pool` on every backend — one body, three dialects (#272, #1461).
//!
//! Replaces `explain_pool_live.rs`, `explain_pool_mysql_live.rs` and
//! `explain_pool_sqlite_live.rs`. Those three shared a model, a query and
//! their assertions, and differed only in pool construction, hand-written
//! DDL, and which marker they grepped for — 382 lines to say one thing
//! three times.
//!
//! What is worth reading here is not the deduplication but how the real
//! differences are handled. `EXPLAIN` genuinely disagrees across engines:
//!
//! | | PostgreSQL | MySQL | SQLite |
//! |---|---|---|---|
//! | JSON shape | array | **object** | array |
//! | `analyze: true` | runs it, reports timings | runs it, reports timings (8.0.18+) | silently ignored |
//!
//! The MySQL cell read "accepted" until #1506 — the pre-correction
//! reading, where the flag is tolerated but the plan stays an estimate.
//! Making the ANALYZE assertion two-sided showed that false, and the
//! arms below were fixed while this table was not. **The arms are
//! authoritative**: they carry the same facts with a `because` string
//! each, and the macro will not compile if one goes missing, which a
//! hand-maintained table cannot promise.
//!
//! A shared body could paper over that by asserting only `!plan.is_empty()`
//! on all three. The suite would stay green and stop testing anything.
//! `by_dialect!` is the alternative: every backend is named, the macro will
//! not compile with one missing, and each arm carries the reason it differs.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Column as _;
use rustango::sql::{explain_pool, Auto, ExplainFormat, ExplainOptions, Pool};
use rustango::{by_dialect, tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "explain_tri_demo")]
#[rustango(app = "explain_pool_tri")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// `max_length` is the point of routing DDL through the emitter: this
    /// is `VARCHAR(64)` on PG and MySQL and `TEXT` on SQLite, and the
    /// three files this replaces each hand-wrote their own guess.
    #[rustango(max_length = 64)]
    pub label: String,
}

fn select_query() -> rustango::core::SelectQuery {
    Demo::objects()
        .where_(Demo::label.eq("alpha"))
        .compile()
        .expect("compile")
}

/// Every engine can produce a text plan, and it names the table queried.
///
/// The table-name assertion is what makes this more than "did not error":
/// an empty string or a generic banner would pass `!is_empty()`.
async fn returns_plan_text(pool: &Pool) {
    let plan = explain_pool(pool, &select_query(), ExplainOptions::default())
        .await
        .expect("explain text");

    assert!(!plan.is_empty(), "expected a non-empty plan");
    assert!(
        plan.to_ascii_lowercase().contains("explain_tri_demo"),
        "the plan should name the table it was built from, got:\n{plan}"
    );
}

/// JSON format parses — and the top-level shape is a real divergence.
async fn returns_plan_json(pool: &Pool) {
    let shape = by_dialect! { pool,
        postgres => "array",
            because "EXPLAIN (FORMAT JSON) wraps the plan tree in a one-element array",
        mysql => "object",
            because "EXPLAIN FORMAT=JSON returns a bare `query_block` object, never an array",
        sqlite => "array",
            because "EXPLAIN QUERY PLAN is rows, serialized as [{id, parent, detail}, ...]",
    };

    let plan = explain_pool(
        pool,
        &select_query(),
        ExplainOptions {
            format: ExplainFormat::Json,
            ..Default::default()
        },
    )
    .await
    .expect("explain json");

    assert!(!plan.is_empty(), "expected a non-empty plan");
    let parsed: serde_json::Value =
        serde_json::from_str(&plan).unwrap_or_else(|e| panic!("plan is not JSON: {e}\n{plan}"));

    let got = if parsed.is_array() {
        "array"
    } else if parsed.is_object() {
        "object"
    } else {
        "scalar"
    };
    assert_eq!(
        got,
        shape.value,
        "unexpected JSON shape on {} — {}\n{plan}",
        pool.dialect().name(),
        shape.why
    );
}

/// `analyze` must never be an error, and on PostgreSQL it must actually
/// do something.
///
/// PostgreSQL and MySQL both execute the plan on `EXPLAIN ANALYZE` and
/// report real timings — the MySQL arm below used to claim otherwise,
/// and the claim survived because the assertion sat in a one-sided `if`
/// that skipped it. SQLite has no equivalent and the framework promises
/// to ignore the flag rather than reject it. Asserting only
/// "did not error" would let a silent PG regression through, which is why
/// the timing marker is per-dialect rather than dropped.
async fn analyze_flag_is_accepted(pool: &Pool) {
    let timings = by_dialect! { pool,
        postgres => true,
            because "EXPLAIN ANALYZE executes the plan and prints `actual time=` per node",
        mysql => true,
            because "the framework does opt in, and MySQL 8.0.18+ executes the query and \
                     reports `actual time=..` per node just as PostgreSQL does. This arm \
                     claimed the opposite — that the plan stayed an estimate — and said so \
                     for as long as the assertion was wrapped in a one-sided `if` that \
                     skipped the check whenever the arm selected false (#1461)",
        sqlite => false,
            because "SQLite has no ANALYZE form of EXPLAIN; the documented behaviour is to \
                     ignore the flag, not to reject it",
    };

    let plan = explain_pool(
        pool,
        &select_query(),
        ExplainOptions {
            analyze: true,
            buffers: true,
            verbose: true,
            format: ExplainFormat::Text,
        },
    )
    .await
    .expect("analyze + buffers must be accepted on every backend");

    assert!(
        !plan.is_empty(),
        "expected a plan even with backend-specific flags set"
    );

    // Both directions, deliberately. This was a one-sided `if`, so the
    // MySQL and SQLite arms of the `by_dialect!` above selected `false`
    // and then asserted nothing at all — the macro named all three
    // backends and two of them bought no coverage, which is the
    // averaging-away it exists to prevent.
    //
    // The negative arm is the load-bearing one here: it is what would
    // notice a backend quietly starting to execute the query on
    // `analyze: true`, which is a behaviour change, not an improvement.
    assert_eq!(
        plan.contains("actual time="),
        timings.value,
        "ANALYZE timings on {}: expected present={}, got present={} — {}\n{plan}",
        pool.dialect().name(),
        timings.value,
        plan.contains("actual time="),
        timings.why
    );
}

tri_dialect_test! {
    model: Demo,
    scenarios: [
        returns_plan_text,
        returns_plan_json,
        analyze_flag_is_accepted,
    ],
}
