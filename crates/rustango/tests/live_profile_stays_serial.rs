//! The `live` nextest profile must keep running one test at a time.
//!
//! The live suites share one database and most drop or truncate the
//! framework tables in setup, so in parallel they fight over the
//! catalog: 758 failures against PostgreSQL, 95 against MySQL, 0 either
//! way serially (#1624). `docs/testing.md` now tells people to run
//! `--profile live`, which makes that one config line load-bearing.
//!
//! Drop it while tidying and the profile silently becomes the default
//! one. The symptom is a wall of failures that look like flaky live
//! suites rather than a config regression, which is a bad afternoon.
//!
//! This reads the file because a TOML profile has no type to hang the
//! invariant on. The real fix is a database per test process, which
//! would make the profile — and this guard — unnecessary.

use std::path::Path;

fn nextest_toml() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
        .join(".config/nextest.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `test-threads` as declared in `[profile.live]`, or `None`.
///
/// Parsed line by line rather than matched as a substring. Both
/// shortcuts were wrong: `find("[profile.live]")` latched onto a
/// *commented* header, and `contains("test-threads = 1")` is satisfied
/// by `test-threads = 16`. Each let the guard pass on the exact
/// regression it exists to catch.
fn live_profile_test_threads(toml: &str) -> Option<u32> {
    let mut in_block = false;
    for raw in toml.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue; // a comment is not a header and not a setting
        }
        if line.starts_with('[') {
            in_block = line == "[profile.live]";
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "test-threads" {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

#[test]
fn live_profile_runs_one_test_at_a_time() {
    let toml = nextest_toml();
    let threads = live_profile_test_threads(&toml);
    assert_eq!(
        threads,
        Some(1),
        "[profile.live] must declare `test-threads = 1`; got {threads:?}. \
         Anything else lets the live suites race again and `--profile live` \
         stops meaning what docs/testing.md says it means."
    );
}

#[test]
fn the_guard_rejects_what_it_is_meant_to_reject() {
    // The two mutations the substring version passed.
    let bumped = "[profile.live]\ntest-threads = 16\n";
    assert_eq!(live_profile_test_threads(bumped), Some(16));

    let only_a_comment =
        "# [profile.live] used to set test-threads = 1\n[profile.ci]\nfail-fast = false\n";
    assert_eq!(
        live_profile_test_threads(only_a_comment),
        None,
        "a commented header must not satisfy the guard"
    );
}

#[test]
fn the_docs_still_point_at_the_profile() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/testing.md")
        .canonicalize()
        .expect("docs/testing.md exists");
    let docs = std::fs::read_to_string(path).expect("read docs/testing.md");
    assert!(
        docs.contains("--profile live"),
        "docs/testing.md stopped naming `--profile live`; either the guidance \
         moved or the profile did, and the two must not drift apart"
    );
}
