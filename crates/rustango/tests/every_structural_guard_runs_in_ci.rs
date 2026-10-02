//! Every structural guard runs on an unlabelled pull request.
//!
//! ## The gap this closes
//!
//! `ci.yml`'s `guards` job header states the principle: these are the
//! structural guards, and *"gating them put them out of reach of the
//! pull requests that need them most"*. The job's list of `--test`
//! targets is how that principle is enforced — and the list is
//! maintained by hand, named nowhere else, and was checked by nothing.
//!
//! So it drifted. Five source-reading guards with no database were
//! absent from it — `every_serve_shuts_down_gracefully`,
//! `live_suites_fail_loudly`, `macro_internals_stays_internal`,
//! `tracing_targets` and `pool_construction`. Each ran only inside
//! `postgres_test`, which is `needs: gate`, so on an unlabelled PR none
//! of them ran at all. All five still passed when this was written, so
//! nothing had rotted — but nothing would have said if it had.
//!
//! That is the same failure as #1437, #1548 and #1572, one level up:
//! the guard reports green by not being asked.
//!
//! ## What counts as a structural guard — a property, not a name
//!
//! A test that
//!
//! 1. derives a path from `env!("CARGO_MANIFEST_DIR")` — the idiom for
//!    "recomputes something from the source tree", as opposed to
//!    exercising the library, and
//! 2. carries **no crate-level `#![cfg(...)]`**, because a guard that
//!    compiles out under the `guards` job's feature set cannot run
//!    there.
//!
//! The first version of this file keyed on the `every_*` / `docs_*`
//! filename convention instead. That closed the hole for one file and
//! left four open while reporting green — `pool_construction` and
//! `tracing_targets` are structural guards whose names say nothing of
//! the sort. It also needed a hard-coded exception for
//! `logging_fields_agree`, which was the signal the convention did not
//! hold.
//!
//! The property selects exactly the same set with no exceptions:
//! `admin_csrf_sqlite_live` uses `CARGO_MANIFEST_DIR` too and is
//! excluded on its own `#![cfg]`, correctly — it needs SQLite.
//!
//! ## Why the job must also be checked
//!
//! Naming every guard in a job that is itself gated proves nothing. Two
//! lines of YAML on `guards:` — `needs: gate`, or
//! `continue-on-error: true` — put every listed guard back out of reach
//! while this file still passed, because an earlier version asserted
//! only that the names were *listed*. It was called
//! `..._is_ungated` and never checked that.

use std::path::{Path, PathBuf};

/// Repository root, derived from this crate rather than the working
/// directory — `cargo test` and an IDE runner disagree about CWD.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
}

/// Does this file carry a crate-level `#![cfg(...)]`?
///
/// The whole file is scanned, not a prefix. A 40-line window missed
/// `every_serving_path_is_observable.rs`, whose gate sits at line 47 —
/// so it was classified as runnable-anywhere while being gated on
/// `config`. Benign only because `config` arrives via `batteries`.
///
/// `#![cfg(` with the paren, so `#![cfg_attr(...)]` — which gates
/// nothing — is not mistaken for a gate.
fn has_crate_gate(src: &str) -> bool {
    src.lines().any(|l| l.trim_start().starts_with("#![cfg("))
}

/// A test that recomputes something from the source tree.
fn reads_the_source_tree(src: &str) -> bool {
    src.contains("CARGO_MANIFEST_DIR")
}

/// Every test in the file is `#[ignore]`d, so it runs nowhere by
/// design and a CI line for it would add zero coverage.
///
/// One file is in this state: `macro_no_backend_cfg`, disabled with a
/// reason dated v0.38 against a crate now at 0.57.x. Naming it in the
/// job would satisfy this guard while running nothing — the exact
/// shape of the problem, so it is excluded rather than swept in. If it
/// is re-enabled it re-enters this guard automatically.
fn every_test_ignored(src: &str) -> bool {
    let tests = src.matches("#[test]").count() + src.matches("#[tokio::test]").count();
    tests > 0 && src.matches("#[ignore").count() >= tests
}

/// The `guards` job's body, comments stripped.
///
/// Trailing comments are cut too, not only whole-line ones: a step
/// carrying `# … --test foo` in prose would otherwise vouch for a
/// guard that is not named. Both sibling parsers strip them and each
/// documents why; the first version of this file dropped that.
fn guards_job(yaml: &str) -> String {
    let start = yaml
        .find("\n  guards:\n")
        .expect("no `guards:` job in ci.yml — renamed, or this guard reads the wrong file")
        + 1;
    let rest = &yaml[start..];
    let mut out = String::new();
    for (i, line) in rest.lines().enumerate() {
        // Stop at the next job header: a key at exactly two spaces,
        // whose name is a plain identifier. The charset check matters —
        // `deny-examples` is a job and a naive rule skipped it.
        if i > 0 {
            if let Some(rest_of) = line.strip_prefix("  ") {
                let name = rest_of.split(':').next().unwrap_or("");
                if !rest_of.starts_with([' ', '#', '-'])
                    && rest_of.contains(':')
                    && !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    break;
                }
            }
        }
        let code = match line.find(" #") {
            Some(i) => &line[..i],
            None => line,
        };
        if code.trim_start().starts_with('#') {
            continue;
        }
        out.push_str(code);
        out.push('\n');
    }
    out
}

/// Is `target` named as a `--test` argument, as a whole word?
///
/// Whole word, not a prefix. `job.contains("--test every_serving")`
/// was satisfied by the listed `--test every_serving_path_is_observable`,
/// so a new `every_serving.rs` would have reported as covered. No pair
/// collides today, but `every_serve_*` and `every_serving_*` show how
/// crowded the namespace already is.
fn names_target(job: &str, target: &str) -> bool {
    job.split("--test ").skip(1).any(|chunk| {
        let name: String = chunk
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        name == target
    })
}

/// Every `tests/*.rs` that is a structural guard, by the property above.
fn structural_guards(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let dir = root.join("crates/rustango/tests");
    for entry in std::fs::read_dir(&dir).expect("tests/ is readable") {
        let path = entry.expect("dir entry").path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        if reads_the_source_tree(&src) && !has_crate_gate(&src) && !every_test_ignored(&src) {
            out.push(stem.to_owned());
        }
    }
    out.sort();
    out
}

#[test]
fn every_structural_guard_is_named_in_the_guards_job() {
    let root = repo_root();
    let yaml =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("ci.yml is readable");
    let job = guards_job(&yaml);

    let missing: Vec<String> = structural_guards(&root)
        .into_iter()
        .filter(|stem| !names_target(&job, stem))
        .collect();

    assert!(
        missing.is_empty(),
        "structural guard(s) not named in the `guards` job: {missing:?}\n\n\
         These recompute a property of the source tree and need no database, so they \
         belong in the ungated job. Left out, they run only inside `postgres_test` \
         (`needs: gate`) and therefore never run on an unlabelled pull request — the \
         state five of them were found in, and the failure mode #1437 / #1548 / #1572 \
         are all instances of.\n\n\
         Add `--test <name>` to the `guards` job in .github/workflows/ci.yml."
    );
}

/// Naming a guard in a job that cannot run, or cannot fail, proves
/// nothing.
///
/// This is the half the first version of this file left out while its
/// own name claimed it. `every_test_suite_compiles_in_ci` carries the
/// same check for its own job, and the shape is deliberately identical.
#[test]
fn the_guards_job_is_actually_ungated() {
    let root = repo_root();
    let yaml =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("ci.yml is readable");
    let job = guards_job(&yaml);

    // `continue-on-error` does not stop the job running, but it stops
    // it mattering — which is the same thing for a guard.
    let gates: Vec<&str> = ["needs:", "if:", "continue-on-error:"]
        .into_iter()
        .filter(|k| job.lines().any(|l| l.trim_start().starts_with(*k)))
        .collect();

    assert!(
        gates.is_empty(),
        "the `guards` job is gated by {gates:?}, so every guard named in it is back \
         out of reach of an unlabelled pull request — which is the entire thing this \
         file exists to prevent. Two lines of YAML here undo every `--test` line below \
         them."
    );

    // A guard that is compiled and not run is the same silent pass.
    assert!(
        !job.contains("--no-run"),
        "the `guards` job runs with `--no-run`, so the guards compile and none of them \
         execute — a green job that asserted nothing."
    );
}

/// The reverse: a name in the job must still exist on disk.
#[test]
fn every_guard_named_in_the_job_still_exists() {
    let root = repo_root();
    let yaml =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("ci.yml is readable");
    let job = guards_job(&yaml);

    let mut absent: Vec<String> = Vec::new();
    for chunk in job.split("--test ").skip(1) {
        let name: String = chunk
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        if !root
            .join("crates/rustango/tests")
            .join(format!("{name}.rs"))
            .exists()
        {
            absent.push(name);
        }
    }
    absent.sort();
    absent.dedup();

    assert!(
        absent.is_empty(),
        "the `guards` job names target(s) with no file: {absent:?} — renamed, deleted, \
         or mistyped in ci.yml"
    );
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn a_trailing_comment_cannot_vouch_for_a_guard() {
        let yaml =
            "\n  guards:\n    steps:\n      - run: cargo fmt  # not --test sneaky\n\n  next:\n";
        assert!(!names_target(&guards_job(yaml), "sneaky"));
    }

    #[test]
    fn a_name_is_matched_whole_not_as_a_prefix() {
        let job = "      - run: cargo test --test every_serving_path_is_observable\n";
        assert!(names_target(job, "every_serving_path_is_observable"));
        assert!(!names_target(job, "every_serving"));
    }

    #[test]
    fn the_block_stops_at_the_next_job_including_hyphenated_ones() {
        let yaml = "\n  guards:\n      - run: cargo test --test a\n\n  deny-examples:\n      - run: cargo test --test b\n";
        let block = guards_job(yaml);
        assert!(names_target(&block, "a"));
        assert!(!names_target(&block, "b"), "read past `deny-examples:`");
    }

    #[test]
    fn cfg_attr_is_not_a_crate_gate() {
        assert!(!has_crate_gate("#![cfg_attr(docsrs, feature(doc_cfg))]\n"));
        assert!(has_crate_gate("#![cfg(feature = \"sqlite\")]\n"));
    }

    #[test]
    fn a_gate_below_the_first_forty_lines_is_still_found() {
        let src = format!("{}#![cfg(feature = \"config\")]\n", "//! doc\n".repeat(46));
        assert!(has_crate_gate(&src), "the scan must not stop at a prefix");
    }

    #[test]
    fn a_fully_ignored_suite_is_not_required_in_ci() {
        assert!(every_test_ignored(
            "#[test]\n#[ignore = \"x\"]\nfn a() {}\n"
        ));
        assert!(!every_test_ignored("#[test]\nfn a() {}\n"));
        assert!(!every_test_ignored(
            "#[test]\n#[ignore]\nfn a() {}\n#[test]\nfn b() {}\n"
        ));
    }
}
