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

/// The `[profile.live]` block, up to the next `[` at column 0.
fn live_profile(toml: &str) -> &str {
    let start = toml
        .find("[profile.live]")
        .expect("`[profile.live]` is gone — docs/testing.md tells people to use it");
    let rest = &toml[start + "[profile.live]".len()..];
    match rest.find("\n[") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

#[test]
fn live_profile_runs_one_test_at_a_time() {
    let toml = nextest_toml();
    let block = live_profile(&toml);
    assert!(
        block.contains("test-threads = 1"),
        "[profile.live] lost `test-threads = 1`, so it no longer serializes \
         the live suites and `--profile live` is a lie. Block was:\n{block}"
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
