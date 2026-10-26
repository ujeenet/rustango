//! No live suite may swallow a broken database (#1440).
//!
//! The idiom this forbids:
//!
//! ```ignore
//! let url = std::env::var("DATABASE_URL").ok()?;   // unset -> skip, correct
//! sqlx::PgPool::connect(&url).await.ok()           // set-but-broken -> skip, BUG
//! ```
//!
//! Unset means "no database configured here", and skipping is right. Set
//! but unreachable means a wrong port, a service that never came up, or a
//! container that died mid-run — and the suite then reports `ok. N passed`
//! having done nothing at all. 66 suites and 204 test functions did that,
//! after #1434 and #1444 fixed the eight ORM scenario files. (60 of them were
//! found by a single-line grep; the last six build the pool through
//! `PoolOptions` across several lines and this guard is what caught them.)
//!
//! Not hypothetical: #1437 turned on 22 media tests that had never
//! executed, and all 22 failed on first contact with a real database —
//! #1450, a live break in media upload. A green wall hides real defects,
//! not just untested code.
//!
//! Recomputed from the tree rather than pinned to a number, so it fails
//! on a suite added tomorrow. Scoped by the **defect's shape**, not by
//! filename: `operator_branding_env.rs` is in the broken set and is not
//! named `*_live.rs`, so a filename sweep would have missed it.

use std::fs;
use std::path::Path;

/// `connect(<anything>).await.ok()` — the Result becomes an Option and
/// the error is gone. Also catches `.ok()?`, which is the same discard.
///
/// Deliberately agnostic about the argument's name: keying on the
/// literal `&url` would let a suite spelling it `&db_url` walk straight
/// past, which would make "a suite added tomorrow cannot reintroduce
/// it" an overstatement rather than a guarantee.
fn swallows_a_connect_error(src: &str) -> bool {
    // Normalise whitespace so a rustfmt line break cannot hide it.
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut rest = flat.as_str();
    while let Some(at) = rest.find("connect(") {
        let after = &rest[at + "connect(".len()..];
        // Step over the argument to the closing paren, then require the
        // discard to follow immediately.
        if let Some(close) = after.find(')') {
            let tail = after[close + 1..].trim_start();
            if tail.starts_with(". await . ok ()")
                || tail.starts_with(".await.ok()")
                || tail.starts_with(". await .ok()")
                || tail.starts_with(".await .ok()")
            {
                return true;
            }
            // `match connect(..).await { Ok(p) => Some(p), Err(e) => None }`.
            if tail.starts_with(".await {") && err_arm_skips(tail) {
                return true;
            }
        }
        rest = &rest[at + "connect(".len()..];
    }
    false
}

/// The `Err` arm of the match starting at `tail` yields `None` or returns
/// without panicking.
fn err_arm_skips(tail: &str) -> bool {
    let Some(at) = tail.find("Err(") else {
        return false;
    };
    let arm = &tail[at..];
    // The arm ends where the match does: the first `}` closing at depth 0.
    let mut depth = 0i32;
    let mut end = arm.len();
    for (i, c) in arm.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => {
                end = i;
                break;
            }
            '}' => depth -= 1,
            _ => {}
        }
    }
    let arm = &arm[..end];
    !arm.contains("panic!") && (arm.contains("None") || arm.contains("return"))
}

/// A `CREATE EXTENSION` whose failure skips instead of going through
/// `testkit::matrix::pg_extension`, which fails in CI.
fn skips_a_missing_extension(src: &str) -> bool {
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.match_indices("CREATE EXTENSION").any(|(at, _)| {
        let window: String = flat[at..].chars().take(160).collect();
        let window = window.replace(' ', "");
        window.contains(".ok()?") || window.contains(".is_err()")
    })
}

#[test]
fn no_test_swallows_an_unreachable_database() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // This file quotes the idiom in its own docs.
        if path.file_name().and_then(|n| n.to_str()) == Some("live_suites_fail_loudly.rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read test file");
        if swallows_a_connect_error(&src) {
            offenders.push(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_owned(),
            );
        }
    }

    offenders.sort();
    assert!(
        offenders.is_empty(),
        "{} test file(s) turn a failed connect into a silent skip, so they report \
         `ok. N passed` against a database that is not there (#1440). Use \
         `.unwrap_or_else(|e| panic!(\"DATABASE_URL is set but unreachable ({{url}}): {{e}}\"))` \
         — skipping is only correct when the env var is *unset*.\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}

#[test]
fn no_test_skips_a_missing_extension_in_ci() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    for entry in fs::read_dir(&dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("live_suites_fail_loudly.rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read test file");
        if skips_a_missing_extension(&src) {
            offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these suites skip when `CREATE EXTENSION` fails, so CI goes green on an \
         image without it. Use `rustango::testkit::matrix::pg_extension`:\n  {}",
        offenders.join("\n  ")
    );
}

mod shape_tests {
    use super::*;

    #[test]
    fn a_match_that_skips_on_connect_error_is_caught() {
        let skip = "match Pool::connect(&url).await {\n Ok(p) => Some(p),\n \
                    Err(e) => {\n eprintln!(\"skip {e}\");\n None\n }\n }";
        assert!(swallows_a_connect_error(skip));
        let loud = "match Pool::connect(&url).await { Ok(p) => Some(p), \
                    Err(e) => panic!(\"down: {e}\"), }";
        assert!(!swallows_a_connect_error(loud));
    }

    #[test]
    fn an_extension_skip_is_caught() {
        assert!(skips_a_missing_extension(
            "sqlx::query(\"CREATE EXTENSION IF NOT EXISTS hstore\")\n .execute(&pg)\n .await\n .ok()?;"
        ));
        assert!(skips_a_missing_extension(
            "if sqlx::query(\"CREATE EXTENSION IF NOT EXISTS vector\") .execute(&pg) .await .is_err() {"
        ));
        assert!(!skips_a_missing_extension(
            "sqlx::query(\"CREATE EXTENSION citext\").execute(&pg).await.unwrap();"
        ));
    }
}
