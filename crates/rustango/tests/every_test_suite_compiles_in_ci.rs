//! Some ungated CI job must **compile** every test suite, whatever
//! features gate it (#1572).
//!
//! ## The hole this guards
//!
//! `default = ["postgres", "batteries"]`, and `batteries` pulls in
//! neither `sqlite` nor `mysql`. A suite gated on one of those —
//! `#![cfg(feature = "sqlite")]` and friends — compiles to an **empty
//! crate** without it. It builds, it reports `ok`, and it contains
//! nothing. Evaluated against the feature set `clippy` resolves, that
//! is **275 of 559** suites in `tests/`.
//!
//! Before this job existed, `clippy` was the only ungated job that
//! compiled `tests/**` at all, and it runs `--features tenancy` on top
//! of the defaults. So on an unlabelled PR into `develop`, those 275
//! were neither run nor built. A change breaking one of their
//! signatures merged with a green tick and first failed post-merge, on
//! develop's push run.
//!
//! That is #1437 and #1548 one layer down. Those are about a suite that
//! *skips* silently; this is about a suite that is never *built* where
//! anyone would look.
//!
//! ## What this asserts, and why each half is needed
//!
//! Some job must build the full test surface, **and** that job must be
//! reachable on an ordinary pull request. Neither alone is the
//! invariant, and both halves have a way of rotting:
//!
//! - **Gating.** The job is the slowest ungated one, so switching it
//!   off will look like an easy saving. This workflow has three ways to
//!   do that — `needs:`, a job-level `if:` (which is how `gate` itself
//!   works, and whose header warns that copies drift) and
//!   `continue-on-error:` (already used by `windows_test`). An earlier
//!   version of this guard checked only `needs:`, so the other two
//!   disabled the job while it still reported `ok`.
//! - **Narrowing.** The command's cost is advertised in its own ci.yml
//!   comment, which makes trimming it at least as likely as gating it.
//!   `--lib`, `--test <one>` or `--exclude rustango` all keep
//!   `--workspace --all-features` on the line while compiling one suite
//!   or none.
//!
//! ## What `--all-features` does *not* cover
//!
//! Six suites are gated on a **negated** feature —
//! `not(feature = "postgres")` for five, `not(feature = "admin")` for
//! `oauth2_router_standalone` — so `--all-features` compiles them out
//! by construction. The second command in `tests_compile` covers the
//! `not(postgres)` family; `oauth2_router_standalone` needs a third
//! shape and is built only in the gated `feature_combos`. That gap is
//! real and stated rather than papered over: this guard asserts the two
//! commands that exist, not that coverage is complete.

use std::path::{Path, PathBuf};

/// Repository root, derived from this crate rather than the working
/// directory — `cargo test` and an IDE runner disagree about CWD.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
}

/// Everything after the top-level `jobs:` key.
///
/// Scoping to it matters: `on:`'s children (`push`, `pull_request`,
/// `workflow_dispatch`) and `concurrency:`'s `group` all sit at the
/// same two-space indent a job header does, so a whole-file scan reads
/// them as jobs. None of them contains a cargo command today, which is
/// the only reason that was harmless.
fn jobs_section(yaml: &str) -> &str {
    let at = yaml
        .find("\njobs:\n")
        .expect("no top-level `jobs:` key in ci.yml — this guard is reading the wrong file");
    &yaml[at + "\njobs:\n".len()..]
}

/// Is this line a job header — a key at exactly two spaces of indent?
///
/// Keys *inside* a job (`runs-on:`, `steps:`, `needs:`) sit at four, so
/// two is unambiguous **within the `jobs:` section**. The hyphen
/// matters: `deny-examples` is a job.
///
/// Trailing content after the colon is tolerated. Requiring the line to
/// *end* at the colon meant a comment on the header line made the whole
/// job unparseable, and its body was then attributed to the job above —
/// so the guard vouched for a job that did not contain the command.
fn job_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("  ")?;
    if rest.starts_with([' ', '#', '-']) {
        return None;
    }
    let name = rest.split(':').next()?.trim_end();
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    // `split(':')` yields the whole line when there is no colon at all,
    // so require one.
    (ok && rest.contains(':')).then_some(name)
}

/// Cut a trailing `# …` comment.
///
/// Whole-line comments are dropped elsewhere; this is the *trailing*
/// case, and it is not cosmetic. Leaving them in let a prose comment on
/// some other job's `run:` line supply the tokens this guard matches
/// on, so deleting the real job and mentioning its command in passing
/// kept the guard green. The sibling `every_mysql_arm_runs_in_ci`
/// strips them for the same reason.
fn strip_trailing_comment(line: &str) -> &str {
    match line.find(" #") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// One job's body: comments removed and `\` continuations joined.
///
/// Joining continuations is what stops this guard failing on the
/// file's own prevailing style. `ci.yml` writes multi-flag cargo
/// invocations as `run: |` with trailing backslashes — the `guards` job
/// is written that way — and a matcher that needs every flag on one
/// physical line goes red on a workflow that is entirely correct. A
/// guard that cries wolf gets deleted, which ends in the same place as
/// one that misses.
fn job_bodies(yaml: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in jobs_section(yaml).lines() {
        if let Some(name) = job_name(line) {
            out.push((name.to_owned(), String::new()));
            continue;
        }
        let Some((_, body)) = out.last_mut() else {
            continue;
        };
        if line.trim_start().starts_with('#') {
            continue;
        }
        let code = strip_trailing_comment(line).trim_end();
        match code.strip_suffix('\\') {
            // A continued line joins the next one instead of ending.
            Some(head) => {
                body.push_str(head);
                body.push(' ');
            }
            None => {
                body.push_str(code);
                body.push('\n');
            }
        }
    }
    out
}

/// Selectors that cancel `--workspace --all-features` by restricting
/// what gets built.
///
/// Matched as whole tokens, not substrings: `--tests` restricts to test
/// targets, which is *wider* than the default for this purpose, and a
/// substring check would reject it along with `--test`.
const NARROWING: &[&str] = &[
    "--lib",
    "--bin",
    "--bins",
    "--test",
    "--example",
    "--examples",
    "--bench",
    "--benches",
    "--doc",
    "--exclude",
];

/// Does this job build the whole workspace with every feature, without
/// narrowing what it builds?
///
/// `--no-run` is deliberately not required: a job that actually ran the
/// suites would satisfy the invariant too, and demanding the exact flag
/// would fail on a future job that does more rather than less.
fn builds_every_suite(body: &str) -> bool {
    body.lines().filter(|l| l.contains("cargo test")).any(|l| {
        let tok: Vec<&str> = l.split_whitespace().collect();
        tok.contains(&"--workspace")
            && tok.contains(&"--all-features")
            && !tok.iter().any(|t| NARROWING.contains(t))
    })
}

/// Every way this workflow can stop a job running on an ordinary PR.
///
/// `continue-on-error:` does not stop it running, but it stops it
/// *mattering*, which is the same thing for a guard job.
const GATES: &[&str] = &["needs:", "if:", "continue-on-error:"];

fn gating_keys(body: &str) -> Vec<&'static str> {
    GATES
        .iter()
        .filter(|k| body.lines().any(|l| l.trim_start().starts_with(**k)))
        .copied()
        .collect()
}

#[test]
fn some_ungated_job_compiles_every_test_suite() {
    let yaml = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .expect("ci.yml is readable");

    let all = job_bodies(&yaml);
    assert!(
        all.iter().any(|(n, _)| n == "gate"),
        "no `gate` job found in ci.yml's `jobs:` section — this guard is reading the wrong \
         file, or the gating model changed and this test needs rewriting rather than deleting"
    );

    let builders: Vec<&(String, String)> = all
        .iter()
        .filter(|(_, body)| builds_every_suite(body))
        .collect();
    assert!(
        !builders.is_empty(),
        "no CI job runs `cargo test --workspace --all-features` without a narrowing selector. \
         275 of 559 suites in tests/ are gated on a feature the default set omits, so they \
         compile to empty crates in the only jobs an unlabelled PR runs — and a test file that \
         does not build merges green (#1572). Note that adding `--lib`, `--test <one>` or \
         `--exclude` to an existing job's command puts it in this state without removing it."
    );

    let ungated: Vec<&str> = builders
        .iter()
        .filter(|(_, body)| gating_keys(body).is_empty())
        .map(|(n, _)| n.as_str())
        .collect();

    assert!(
        !ungated.is_empty(),
        "every job building the full test surface is gated: {:?}. A gated job does not run on \
         an unlabelled pull request, which is exactly where a suite that does not compile slips \
         through, so this reports green by not looking (#1572).\n\nAdd an ungated job that \
         builds it — do NOT ungate one of the above: `postgres_test` and friends need a live \
         database and belong in the gated matrix, and dropping their `needs:` would run the \
         whole live suite on every pull request.",
        builders
            .iter()
            .map(|(n, body)| format!("{n} (gated by {:?})", gating_keys(body)))
            .collect::<Vec<_>>()
    );
}
