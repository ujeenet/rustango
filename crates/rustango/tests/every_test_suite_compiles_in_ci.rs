//! Some ungated CI job must **compile** every test suite, whatever
//! features gate it (#1572).
//!
//! ## The hole this guards
//!
//! `default = ["postgres", "batteries"]`, and `batteries` pulls in
//! neither `sqlite`, `testkit` nor `mysql`. A suite gated on one of
//! those — `#![cfg(feature = "sqlite")]` and friends — compiles to an
//! **empty crate** under default features. It builds, it reports `ok`,
//! and it contains nothing.
//!
//! Before this job existed, `clippy` was the only ungated job that
//! compiled `tests/**` at all, and it runs `--features tenancy` on top
//! of the defaults. So on an unlabelled PR into `develop`, six suites —
//! including every guard the 0.57.7 security pass added — were neither
//! run nor built. A change breaking one of their signatures merged with
//! a green tick, and first failed post-merge on develop's push run.
//!
//! That is #1437 and #1548 one layer down. Those are about a suite that
//! *skips* silently; this is about a suite that is never *built* where
//! anyone would look.
//!
//! ## Why the assertion is about gating, not about the command
//!
//! The fix is one job. The way the fix rots is not someone deleting it —
//! it is someone adding `needs: gate` to it, because it is the slowest
//! ungated job and that will look like an easy saving. The moment it is
//! gated it stops running on exactly the pull requests it exists for,
//! and goes back to reporting green by not looking.
//!
//! So this guard asserts two things about whichever job does the work:
//! that a full-feature workspace test build is named at all, and that
//! the job naming it has no `needs:`. Neither alone is the invariant.
//!
//! ## Why not a glob or a list of suites
//!
//! `every_mysql_arm_runs_in_ci` has to enumerate targets, because MySQL
//! suites need different feature sets and a glob cannot express that.
//! This one does not: `--workspace --all-features` is by construction
//! every suite under every feature, so there is no list to fall behind
//! the directory. That is the whole reason to prefer it over a curated
//! job.

use std::path::{Path, PathBuf};

/// Repository root, derived from this crate rather than the working
/// directory — `cargo test` and an IDE runner disagree about CWD.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
}

/// Is this line a job header — a key at exactly two spaces of indent?
///
/// Keys *inside* a job (`runs-on:`, `steps:`, `needs:`) sit at four, so
/// two is unambiguous. The hyphen matters: `deny-examples` is a job.
fn is_job_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("  ") else {
        return false;
    };
    if rest.starts_with([' ', '#', '-']) {
        return false;
    }
    let Some(name) = rest.trim_end().strip_suffix(':') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Every job in `ci.yml`, as `(name, body)`.
///
/// Comment lines are dropped from the body. Several job headers in this
/// file explain *other* jobs' gating in prose — the `gate` job's own
/// comment names six ungated jobs — and a body that carries that text
/// would vouch for a job that does not contain the command at all.
fn jobs(yaml: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in yaml.lines() {
        if is_job_header(line) {
            let name = line.trim().trim_end_matches(':').to_owned();
            out.push((name, String::new()));
        } else if let Some((_, body)) = out.last_mut() {
            if !line.trim_start().starts_with('#') {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    out
}

/// Does this job body build the whole workspace with every feature?
///
/// `--no-run` is not required: a job that actually *ran* them would
/// satisfy the invariant too, and demanding the exact flag would make
/// this guard fail on a future job that does more rather than less.
fn builds_every_suite(body: &str) -> bool {
    body.lines()
        .filter(|l| l.contains("cargo test"))
        .any(|l| l.contains("--workspace") && l.contains("--all-features"))
}

#[test]
fn some_ungated_job_compiles_every_test_suite() {
    let yaml = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .expect("ci.yml is readable");

    let all = jobs(&yaml);
    assert!(
        all.iter().any(|(n, _)| n == "gate"),
        "no `gate` job in ci.yml — this guard is reading the wrong file, or the \
         gating model changed and this test needs rewriting rather than deleting"
    );

    let builders: Vec<&(String, String)> = all
        .iter()
        .filter(|(_, body)| builds_every_suite(body))
        .collect();
    assert!(
        !builders.is_empty(),
        "no CI job runs `cargo test --workspace --all-features`. Every suite \
         gated on `sqlite`, `testkit` or `mysql` — which is every guard the \
         media and migrate work added — then compiles to an empty crate in the \
         only jobs an unlabelled PR runs, and a test file that does not build \
         merges green (#1572)"
    );

    let ungated: Vec<&str> = builders
        .iter()
        .filter(|(_, body)| !body.lines().any(|l| l.trim_start().starts_with("needs:")))
        .map(|(n, _)| n.as_str())
        .collect();

    assert!(
        !ungated.is_empty(),
        "every job building the full test surface is gated behind `needs: gate`: \
         {:?}. Gated jobs do not run on an unlabelled pull request, which is \
         exactly where a suite that does not compile slips through — so this \
         reports green by not looking. Drop `needs:` from one of them (#1572)",
        builders.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
    );
}
