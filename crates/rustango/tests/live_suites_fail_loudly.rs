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
//! having done nothing at all. 60 suites and 204 test functions did that,
//! after #1434 and #1444 fixed the eight django6 files.
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

/// `connect(&url).await.ok()` — the Result becomes an Option and the
/// error is gone. Also catches `.ok()?`, which is the same discard.
fn swallows_a_connect_error(src: &str) -> bool {
    // Normalise whitespace so a rustfmt line break cannot hide it.
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    for marker in ["connect(&url) . await . ok ()", "connect(&url).await.ok()"] {
        if flat.contains(marker) {
            return true;
        }
    }
    // The shape rustfmt actually produces once joined.
    flat.contains("connect(&url) .await .ok()") || flat.contains("connect(&url).await .ok()")
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
