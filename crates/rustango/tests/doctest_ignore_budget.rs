//! `ignore` doctests compile nothing, so their number may only shrink.
//!
//! Write a new example as a doctest that runs, `no_run`, or a test; then
//! lower [`IGNORE_BUDGET`] whenever you turn an `ignore` into one of those.

use std::path::Path;

/// `ignore` doctest fences under `crates/*/src` (2026-11-04, 0.62.0).
const IGNORE_BUDGET: usize = 589;

/// Whether `tags` (the text after ```` ``` ````) include `ignore`.
fn has_ignore_tag(tags: &str) -> bool {
    tags.split(',').any(|tag| tag.trim() == "ignore")
}

/// An `ignore` fence in a `///` or `//!` line, or a `#[doc = "…"]` attribute.
fn is_ignore_fence(line: &str) -> bool {
    let t = line.trim_start();
    let rest = t
        .strip_prefix("///")
        .or_else(|| t.strip_prefix("//!"))
        .or_else(|| t.strip_prefix("#[doc = \""))
        .or_else(|| t.strip_prefix("#![doc = \""));
    let Some(rest) = rest else {
        return false;
    };
    rest.trim_start()
        .strip_prefix("```")
        .is_some_and(|tags| has_ignore_tag(tags.trim_end_matches(['"', ']'])))
}

/// `ignore` fences in a file, including inside `/** */` and `/*! */` blocks.
fn count_in(text: &str) -> usize {
    let mut n = 0;
    let mut in_block = false;
    for line in text.lines() {
        let t = line.trim_start();
        if !in_block && (t.starts_with("/**") || t.starts_with("/*!")) && !t.starts_with("/**/") {
            in_block = true;
        }
        if in_block {
            let inner = t.trim_start_matches(['/', '*', '!']).trim_start();
            if inner.strip_prefix("```").is_some_and(has_ignore_tag) {
                n += 1;
            }
            if t.contains("*/") {
                in_block = false;
            }
        } else if is_ignore_fence(line) {
            n += 1;
        }
    }
    n
}

fn count(dir: &Path) -> usize {
    let mut n = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            n += count(&path);
        } else if path.extension().is_some_and(|e| e == "rs") {
            n += count_in(&std::fs::read_to_string(&path).unwrap());
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
    assert!(is_ignore_fence("#[doc = \"```ignore\"]"));
    assert_eq!(
        count_in("/**\n * ```ignore\n * x\n * ```\n */\nfn f() {}\n"),
        1
    );
    assert_eq!(count_in("/*! ```rust,ignore\n```\n*/\n"), 1);
    assert_eq!(count_in("let g = \"a/**/b\";\n/// ```text\n"), 0);
}
