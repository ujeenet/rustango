//! `ignore` doctests compile nothing, so their number may only shrink.
//!
//! Write a new example as a doctest that runs, `no_run`, or a test; then
//! lower [`IGNORE_BUDGET`] whenever you turn an `ignore` into one of those.

use std::path::Path;

/// `ignore` doctest fences under `crates/*/src` (2026-11-04, 0.62.0).
const IGNORE_BUDGET: usize = 589;

/// An ```` ``` ```` fence in a `///` or `//!` comment whose tags include `ignore`.
fn is_ignore_fence(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")) else {
        return false;
    };
    let Some(tags) = rest.trim_start().strip_prefix("```") else {
        return false;
    };
    tags.split(',').any(|tag| tag.trim() == "ignore")
}

fn count(dir: &Path) -> usize {
    let mut n = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            n += count(&path);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            n += text.lines().filter(|l| is_ignore_fence(l)).count();
        }
    }
    n
}

#[test]
fn ignore_doctests_only_shrink() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut n = 0;
    for entry in std::fs::read_dir(&crates).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            n += count(&src);
        }
    }
    assert!(
        n <= IGNORE_BUDGET,
        "{n} `ignore` doctests, budget {IGNORE_BUDGET}: make the new one run, `no_run`, or a test"
    );
    assert_eq!(
        n, IGNORE_BUDGET,
        "`ignore` doctests went down to {n}: lower IGNORE_BUDGET to match"
    );
}

#[test]
fn the_fence_matcher_reads_tags() {
    assert!(is_ignore_fence("/// ```ignore"));
    assert!(is_ignore_fence("    //! ```rust,ignore"));
    assert!(!is_ignore_fence("/// ```no_run"));
    assert!(!is_ignore_fence("/// ```text"));
    assert!(!is_ignore_fence("// ```ignore"));
}
