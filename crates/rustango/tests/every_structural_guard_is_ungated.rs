//! Every structural guard must run on an unlabelled pull request.
//!
//! ## The gap this closes
//!
//! `ci.yml`'s `guards` job header states the principle: these are the
//! structural guards, and *"gating them put them out of reach of the
//! pull requests that need them most"*. The job's list of `--test`
//! targets is how that principle is enforced — and the list is
//! maintained by hand, named nowhere else, and checked by nothing.
//!
//! So it drifted. `every_serve_shuts_down_gracefully` is a
//! source-reading guard with no database and no feature gate, and it was
//! absent from the list: it ran only inside `postgres_test`, which is
//! `needs: gate`, so on an unlabelled PR it never ran at all. It still
//! passed when this was found, so nothing had rotted yet — but nothing
//! would have said if it had.
//!
//! That is the same failure as #1437, #1548 and #1572, one level up: the
//! guard reports green by not being asked.
//!
//! ## What counts as a structural guard
//!
//! A test in `crates/rustango/tests/` that
//!
//! - is named `every_*.rs`, `docs_*.rs`, or `logging_fields_agree.rs` —
//!   the families this repo uses for source- and config-recomputing
//!   guards, and
//! - carries **no crate-level `#![cfg(...)]`**, because a guard that
//!   compiles out under the `guards` job's feature set cannot run there.
//!
//! The naming convention is doing real work: it is the only signal that
//! separates "recomputes a property of the tree" from "exercises the
//! ORM". A guard named outside those families is invisible here, which
//! is a known limit rather than an oversight — see the note at the end
//! of this file.
//!
//! ## Why not assert the list verbatim
//!
//! Pinning the exact set would fail on every added guard, and the fix
//! would be to edit this test — which teaches people to edit it, which
//! is how a ratchet stops ratcheting. It asserts the direction that
//! matters instead: nothing on disk is missing from the job.

use std::path::{Path, PathBuf};

/// Repository root, derived from this crate rather than the working
/// directory — `cargo test` and an IDE runner disagree about CWD.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
}

/// Does this file name put it in a guard family?
fn is_guard_name(stem: &str) -> bool {
    stem.starts_with("every_") || stem.starts_with("docs_") || stem == "logging_fields_agree"
}

/// The `guards` job's body, comments stripped.
///
/// Comments are dropped because the header above the job discusses
/// other jobs by name, and a guard mentioned in prose must not count as
/// wired — the same trap `every_test_suite_compiles_in_ci` hit.
fn guards_job(yaml: &str) -> String {
    let start = yaml
        .find("\n  guards:\n")
        .expect("no `guards:` job in ci.yml — renamed, or this guard reads the wrong file")
        + 1;
    let rest = &yaml[start..];
    let mut out = String::new();
    for (i, line) in rest.lines().enumerate() {
        // Stop at the next job header: a key at exactly two spaces.
        if i > 0
            && line.starts_with("  ")
            && !line[2..].starts_with([' ', '#', '-'])
            && line.contains(':')
        {
            break;
        }
        if !line.trim_start().starts_with('#') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[test]
fn every_structural_guard_is_named_in_the_ungated_job() {
    let root = repo_root();
    let yaml =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("ci.yml is readable");
    let job = guards_job(&yaml);

    let mut missing: Vec<String> = Vec::new();
    let dir = root.join("crates/rustango/tests");
    for entry in std::fs::read_dir(&dir).expect("tests/ is readable") {
        let path = entry.expect("dir entry").path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("rs") || !is_guard_name(stem) {
            continue;
        }
        // A crate-level `#![cfg]` means it can compile out under the
        // job's feature set, so it is not a candidate for this job.
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        if src.lines().take(40).any(|l| l.starts_with("#![cfg")) {
            continue;
        }
        if !job.contains(&format!("--test {stem}")) {
            missing.push(stem.to_owned());
        }
    }
    missing.sort();

    assert!(
        missing.is_empty(),
        "structural guard(s) not named in the `guards` job: {missing:?}\n\n\
         These recompute a property of the source tree and need no database, so they \
         belong in the ungated job. Left out, they run only inside `postgres_test` \
         (`needs: gate`) and therefore never run on an unlabelled pull request — which \
         is the state `every_serve_shuts_down_gracefully` was found in, and the failure \
         mode #1437 / #1548 / #1572 are all instances of.\n\n\
         Add `--test <name>` to the `guards` job in .github/workflows/ci.yml."
    );
}

/// The reverse: a name in the job must still exist on disk.
///
/// Without this the list rots the other way — a renamed or deleted
/// guard leaves a `--test` line that fails the job loudly, which is at
/// least visible, but a *typo* in a name added by hand fails the same
/// way and reads as a broken test rather than a broken list.
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

// Known limit, stated rather than hidden: this keys off the `every_*` /
// `docs_*` naming convention, so a structural guard named outside those
// families is invisible to it. That is a weaker property than "every
// database-free test is ungated", which is not computable here — plenty
// of ordinary tests need no database and belong in the gated matrix all
// the same. The convention is the signal; if a guard is worth writing,
// it is worth naming so.
