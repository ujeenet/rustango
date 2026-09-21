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
        }
        rest = &rest[at + "connect(".len()..];
    }
    false
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
