//! `__regex` / `__iregex` on every backend — one body, three dialects
//! (#26, #1461).
//!
//! Replaces `regex_live.rs` (PostgreSQL) and `regex_sqlite_live.rs`
//! (SQLite). The two files were not testing the same thing at all, and
//! that is the point of merging them:
//!
//! | | PostgreSQL | MySQL | SQLite |
//! |---|---|---|---|
//! | operator | `~` / `~*` / `!~` | `REGEXP` | `REGEXP` → user function |
//! | available? | yes | yes | **no** |
//! | `regex` case | sensitive | **collation-dependent** | n/a |
//!
//! PostgreSQL asserted that matching works. SQLite asserted that it
//! fails *cleanly* — SQLite's `REGEXP` delegates to a `regexp(pattern,
//! value)` user function, and sqlx does not register one because its
//! `regexp` cargo feature is off. The error must therefore come from
//! function resolution, not from the parser: that is what proves the
//! dialect emitted SQL SQLite accepts right up to the last step.
//!
//! Merging them keeps both claims and writes down why they differ, which
//! is what `by_dialect!` is for. Flattening these into "the query does
//! not panic" would have been the easy move and would have deleted the
//! entire content of both suites.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{by_dialect, tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "regex_tri_user")]
#[rustango(app = "regex_tri")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
}

/// Six rows spanning both cases of three stems, so a case-sensitive
/// match and a case-insensitive one give visibly different answers.
async fn seeded(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<User>(pool).await;
    for name in ["alice", "Alice-2", "bob", "Bob-3", "admin", "ADMIN-root"] {
        let mut u = User {
            id: Auto::default(),
            name: name.into(),
        };
        u.insert_pool(pool).await.expect("seed row");
    }
}

fn names(rows: &[User]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|u| u.name.clone()).collect();
    v.sort();
    v
}

/// Run a regex query, asserting the expected names — or, where the
/// backend has no regex at all, that it fails at *function resolution*
/// rather than at the parser.
///
/// The error distinction is the whole of the SQLite suite this replaces:
/// `no such function: REGEXP` means the dialect emitted SQL SQLite
/// parsed happily and only then could not resolve. `near "REGEXP":
/// syntax error` would mean the emitter is producing something SQLite
/// cannot read, which is a different bug entirely.
async fn expect_regex(rows: Result<Vec<User>, rustango::sql::ExecError>, want: Option<Vec<&str>>) {
    match want {
        Some(expected) => {
            let rows = rows.expect("regex query should succeed on this backend");
            let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(names(&rows), want);
        }
        None => {
            let err = rows.expect_err("this backend has no regex support, so this must error");
            let msg = format!("{err}").to_lowercase();
            assert!(
                msg.contains("regexp") || msg.contains("function"),
                "expected a missing-function error naming regexp, got: {err}"
            );
            assert!(
                !msg.contains("syntax error"),
                "a syntax error means the emitter produced SQL this backend cannot \
                 parse, which is a different and worse bug than a missing function: {err}"
            );
        }
    }
}

/// Case-sensitive `regex`.
async fn regex_is_case_sensitive_where_the_backend_allows_it(pool: &Pool) {
    let want = by_dialect! { pool,
        postgres => Some(vec!["alice"]),
            because "the `~` operator is case-sensitive, so `^al.*` misses `Alice-2`",
        mysql => Some(vec!["Alice-2", "alice"]),
            because "MySQL's REGEXP follows the column collation, and the default \
                     utf8mb4_0900_ai_ci is case-insensitive — so REGEXP matches both \
                     cases and `regex` behaves like `iregex` here",
        sqlite => None,
            because "SQLite's REGEXP delegates to a `regexp()` user function that sqlx \
                     does not register (its `regexp` cargo feature is off), so this \
                     fails at function resolution",
    };

    let rows = User::objects()
        .where_(User::name.regex("^al.*"))
        .fetch(pool)
        .await;
    expect_regex(rows, want.value).await;
}

/// Case-insensitive `iregex`.
async fn iregex_matches_both_cases(pool: &Pool) {
    let want = by_dialect! { pool,
        postgres => Some(vec!["Alice-2", "alice"]),
            because "`~*` is the case-insensitive operator",
        mysql => Some(vec!["Alice-2", "alice"]),
            because "same answer as `regex` on a case-insensitive collation — the \
                     LOWER() wrap is redundant here rather than wrong",
        sqlite => None,
            because "the LOWER() wrap still resolves to the same unregistered \
                     `regexp()` function; what matters is that it is a resolution \
                     error and not a mis-quoted LOWER call",
    };

    let rows = User::objects()
        .where_(User::name.iregex("^al.*"))
        .fetch(pool)
        .await;
    expect_regex(rows, want.value).await;
}

/// Negated `regex` — the operator's other half.
async fn not_regex_excludes_matches(pool: &Pool) {
    let want = by_dialect! { pool,
        postgres => Some(vec!["ADMIN-root", "Alice-2", "Bob-3", "alice", "bob"]),
            because "`!~` is case-sensitive, so only the lowercase `admin` is excluded",
        mysql => Some(vec!["Alice-2", "Bob-3", "alice", "bob"]),
            because "NOT REGEXP inherits the case-insensitive collation, so both \
                     `admin` and `ADMIN-root` are excluded",
        sqlite => None,
            because "no regexp function to negate",
    };

    let rows = User::objects()
        .where_(User::name.not_regex("^admin"))
        .fetch(pool)
        .await;
    expect_regex(rows, want.value).await;
}

tri_dialect_test! {
    setup: seeded,
    scenarios: [
        regex_is_case_sensitive_where_the_backend_allows_it,
        iregex_matches_both_cases,
        not_regex_excludes_matches,
    ],
}
