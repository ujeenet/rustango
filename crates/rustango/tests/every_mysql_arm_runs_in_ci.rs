//! Every suite with a MySQL arm — `tests/*_mysql_live.rs` and
//! `tests/*_tri.rs` — must be named in the `mysql_live` CI job (#1461).
//!
//! ## Why this needs a guard at all
//!
//! The three backends are not symmetric in CI, and the asymmetry is
//! invisible from inside a test file:
//!
//! | arm | how it runs | cost of a new suite |
//! |---|---|---|
//! | PostgreSQL | `postgres_test` runs `cargo test --workspace --all-features` with `DATABASE_URL` set | nothing — it is picked up automatically |
//! | SQLite | same job, in-memory | nothing |
//! | **MySQL** | `mysql_live` names every target one at a time | **a line in `ci.yml`, or it never runs** |
//!
//! `MYSQL_TEST_URL` is unset in every other job, and a MySQL suite whose
//! URL is unset skips silently by design (#1440). So a `*_mysql_live.rs`
//! file that nobody added to the allowlist compiles, reports `ok`, and
//! tests nothing — for as long as it exists.
//!
//! That is not hypothetical. `migrate_fake_initial_mysql_live.rs` and
//! `migrate_reconcile_mysql_live.rs` were written, committed, and then
//! ran in no workflow at all. The second one's own header says "MySQL is
//! the strict dialect here … this is where a broken reconcile actually
//! bites", which is exactly right and exactly why nobody noticed it was
//! never asked.
//!
//! This is #1437 repeating: a suite reported green because it was never
//! executed. A glob target (`--test '*_mysql_live'`) would not fix it —
//! the suites need different feature sets (`tenancy`, `mcp`,
//! `jobs-postgres`), so the list has to stay explicit. What can be
//! automated is noticing when the list falls behind the directory.
//!
//! ## The `_tri` half is the sharper edge
//!
//! A `*_mysql_live.rs` suite with no CI line does nothing at all, which
//! at least shows up as a suspiciously fast `0 passed`. A `*_tri.rs`
//! suite with no CI line still runs its PostgreSQL and SQLite arms in
//! `postgres_test` and reports a healthy pass count — while the one arm
//! it was converted to gain never executes.
//!
//! `values_tri`, `regex_tri` and `testkit_matrix_forms_tri` shipped
//! exactly that way: three suites whose entire purpose is to run on all
//! three backends, running on two, looking green. So the guard watches
//! both families.

use std::collections::BTreeSet;
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
/// Keys *inside* a job (`runs-on:`, `steps:`, `env:`) sit at four, so two
/// is unambiguous. The hyphen matters: `deny-examples` is a job, and an
/// earlier version of this parse used `[a-z_0-9]+` and silently skipped
/// it.
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

/// The text of one job, from its header to the next job's.
fn job_block(yaml: &str, job: &str) -> String {
    let needle = format!("\n  {job}:\n");
    let start = yaml
        .find(&needle)
        .unwrap_or_else(|| panic!("no `{job}:` job in ci.yml — this guard is reading the wrong file or the job was renamed"))
        + 1;
    let rest = &yaml[start..];

    let mut offset = 0usize;
    for (i, line) in rest.lines().enumerate() {
        if i > 0 && is_job_header(line) {
            return rest[..offset].to_owned();
        }
        offset += line.len() + 1;
    }
    rest.to_owned()
}

/// Every `--test <target>` named anywhere in a block.
///
/// Scans the raw text rather than only `- run:` lines, so a target inside
/// a multi-line `run: |` script or after a `\` continuation still counts.
fn named_test_targets(block: &str) -> BTreeSet<String> {
    block
        .match_indices("--test ")
        .map(|(i, m)| {
            block[i + m.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Does a suite with this target name have a MySQL arm to run?
///
/// Two families do:
///
/// * `mysql_live` and the `<thing>_mysql_live` family — MySQL-only
///   suites, which do nothing at all without the URL.
/// * the `<thing>_tri` family — tri-dialect suites, whose PostgreSQL and
///   SQLite arms run for free in `postgres_test` but whose `tri_mysql`
///   arm needs `MYSQL_TEST_URL` and therefore needs this job.
///
/// The `_tri` half is not hypothetical either. When this guard was first
/// written it watched only `mysql_live`, and `values_tri`, `regex_tri`
/// and `testkit_matrix_forms_tri` had already shipped with no MySQL line
/// — three converted suites whose whole purpose is to run on all three
/// backends, running on two.
fn has_a_mysql_arm(stem: &str) -> bool {
    stem.ends_with("mysql_live") || stem.ends_with("_tri")
}

/// Every integration-test target that has a MySQL arm.
///
/// Only top-level `.rs` files, which is exactly Cargo's auto-target rule.
fn suites_with_a_mysql_arm() -> BTreeSet<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        .filter(|stem| has_a_mysql_arm(stem))
        .collect()
}

fn mysql_live_job() -> String {
    let path = repo_root().join(".github/workflows/ci.yml");
    let yaml = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    job_block(&yaml, "mysql_live")
}

/// The guard.
#[test]
fn every_suite_with_a_mysql_arm_is_named_in_the_ci_job() {
    let on_disk = suites_with_a_mysql_arm();
    let block = mysql_live_job();
    let named = named_test_targets(&block);

    // Both sides must be non-empty, or the comparison below is vacuous
    // and this file becomes the thing it exists to prevent.
    assert!(
        !on_disk.is_empty(),
        "found no `tests/*mysql_live.rs` or `tests/*_tri.rs` files — the directory \
         scan is broken, so a pass here would mean nothing"
    );
    assert!(
        !named.is_empty(),
        "parsed no `--test` targets out of the `mysql_live` job — the block \
         parse is broken, so a pass here would mean nothing.\n\n{block}"
    );

    let missing: Vec<&String> = on_disk.difference(&named).collect();
    assert!(
        missing.is_empty(),
        "these suites have a MySQL arm that is named in no CI step, so it skips on \
         an unset MYSQL_TEST_URL and reports green without ever running:\
         \n\n  {}\n\nA `*_mysql_live` suite does nothing at all without the URL; a \
         `*_tri` suite still runs its PostgreSQL and SQLite arms in `postgres_test`, \
         which is what makes a missing line here so easy to overlook — the suite \
         looks covered.\n\nAdd a `- run: cargo test -p rustango --features \
         mysql,<...> --test <name>` line to the `mysql_live` job in \
         .github/workflows/ci.yml.",
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// And the reverse: a renamed file must not leave a dangling CI step.
///
/// `cargo test --test <gone>` does fail, so CI would catch this on its
/// own — but it fails as "no target named …" in the middle of a MySQL
/// job, which reads like an infrastructure problem. Naming it here makes
/// a rename obvious at the point it happens.
#[test]
fn every_named_target_with_a_mysql_arm_still_exists() {
    let on_disk = suites_with_a_mysql_arm();
    let block = mysql_live_job();
    let named = named_test_targets(&block);

    let dangling: Vec<&String> = named
        .iter()
        .filter(|n| has_a_mysql_arm(n))
        .filter(|n| !on_disk.contains(*n))
        .collect();
    assert!(
        dangling.is_empty(),
        "the `mysql_live` job names targets with no matching \
         `tests/<name>.rs`:\n\n  {}\n\nThe file was renamed or deleted; update \
         .github/workflows/ci.yml to match.",
        dangling
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Tests for the parse above, so a green guard means the parse worked
/// rather than that it matched nothing.
mod parser_tests {
    use super::*;

    const SAMPLE: &str = "\
name: CI
on:
  push:
    branches: [main]
jobs:
  fmt:
    runs-on: ubuntu-latest
    steps:
      - run: cargo fmt --all -- --check
  mysql_live:
    runs-on: ubuntu-latest
    env:
      MYSQL_TEST_URL: mysql://x
    steps:
      - run: cargo test -p rustango --features mysql --test alpha_mysql_live
      - run: |
          cargo test -p rustango --features mysql,tenancy --test beta_mysql_live
      - run: cargo test -p rustango --all-features \\
          --test gamma_mysql_live
  deny-examples:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test --test not_mine_mysql_live
";

    #[test]
    fn the_block_stops_at_the_next_job() {
        let block = job_block(SAMPLE, "mysql_live");
        assert!(block.contains("alpha_mysql_live"));
        assert!(
            !block.contains("not_mine_mysql_live"),
            "the block ran past `deny-examples` — a hyphenated job name is not \
             a job header to this parser:\n{block}"
        );
        assert!(
            !block.contains("cargo fmt"),
            "the block started too early:\n{block}"
        );
    }

    #[test]
    fn targets_are_found_in_every_run_shape() {
        let found = named_test_targets(&job_block(SAMPLE, "mysql_live"));
        assert!(
            found.contains("alpha_mysql_live"),
            "plain `- run:`: {found:?}"
        );
        assert!(
            found.contains("beta_mysql_live"),
            "`run: |` block: {found:?}"
        );
        assert!(
            found.contains("gamma_mysql_live"),
            "after a `\\` continuation: {found:?}"
        );
    }

    #[test]
    fn hyphenated_job_names_are_headers() {
        assert!(is_job_header("  deny-examples:"));
        assert!(is_job_header("  mysql_live:"));
        assert!(!is_job_header("    runs-on: ubuntu-latest"));
        assert!(!is_job_header("      - run: cargo test"));
        assert!(!is_job_header("  # a comment"));
    }
}
