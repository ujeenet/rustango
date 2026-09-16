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

use std::collections::{BTreeMap, BTreeSet};
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

/// One `- run:` command, ending before the next step or comment.
///
/// Splitting matters: the check below asks whether the command that
/// names a target also enables the `mysql` feature, and that question
/// only means anything per command. Trailing comments are cut because
/// several of them mention `--features mysql` in prose, which would
/// otherwise vouch for the step that follows.
fn run_commands(block: &str) -> Vec<String> {
    // Commented-out steps are not steps. Splitting the raw text on
    // `- run:` matched inside `# - run: cargo test …` too, so disabling
    // a step by commenting it out left the guard satisfied by a line
    // that runs nothing — a suite could be switched off and stay
    // "covered". Strip comment lines before splitting.
    let code: String = block
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        // A step may be written `- name: …` on one line and `run:` on
        // the next — `mysql_live` has one already. Splitting only on
        // `- run:` skipped those entirely, so a `--test` target added to
        // a named step would be invisible to this guard. Normalising the
        // bare `run:` key to the list form makes both shapes one shape.
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("run:") {
                format!("      - {t}")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    code.split("- run:")
        .skip(1)
        .map(|chunk| {
            // Stop at the next *step*, not at any line that happens to
            // begin with `- `. A `run: |` block is shell, and shell
            // contains such lines — a heredoc, an `echo` of a bullet
            // list. Truncating there cut the command short and lost any
            // `--test` after it. A step starts at a shallower indent
            // than the script it follows, so compare columns.
            let mut cmd = String::new();
            let mut script_indent: Option<usize> = None;
            for line in chunk.lines() {
                let indent = line.len() - line.trim_start().len();
                let t = line.trim_start();
                if t.starts_with("- ") {
                    // A list item at or above the step's own indent ends
                    // this command; one nested deeper is script content.
                    match script_indent {
                        Some(base) if indent > base => {}
                        _ => break,
                    }
                } else if !t.is_empty() && script_indent.is_none() {
                    script_indent = Some(indent);
                }
                cmd.push_str(line);
                cmd.push('\n');
            }
            cmd
        })
        .collect()
}

/// Does this command compile the `mysql` feature in?
///
/// `--all-features` does. So does any `--features` list carrying `mysql`
/// as a whole token — `mysql,testkit` yes, `mysqlish` no.
fn enables_mysql(cmd: &str) -> bool {
    if cmd.contains("--all-features") {
        return true;
    }
    // `--features` and its short form `-F`, and the list may be quoted:
    // `--features "mysql,testkit"` is the same thing as the bare form,
    // and reading the quotes as part of the feature name made a correct
    // workflow look like a missing MySQL arm. These fail *closed* —
    // reporting a gap that is not there — which is the safer direction
    // but still a false alarm someone has to chase.
    ["--features ", "--features=", "-F ", "-F="]
        .iter()
        .flat_map(|flag| cmd.match_indices(flag).map(|(i, m)| i + m.len()))
        .any(|start| {
            cmd[start..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .filter(|c| !matches!(c, '"' | '\''))
                .collect::<String>()
                .split(',')
                .any(|f| f == "mysql")
        })
}

/// Every `--test <target>` in the block, and whether the command naming
/// it actually builds with MySQL.
///
/// The `bool` is the point. An earlier version of this guard collected
/// target names and nothing else, so a suite named in a step that did
/// not enable `mysql` satisfied it while compiling no `tri_mysql` module
/// and running no MySQL arm — the guard's own failure mode, one level
/// down. Checking the name is checking a proxy; checking the name
/// alongside the feature set is checking the thing.
fn named_test_targets(block: &str) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for cmd in run_commands(block) {
        let mysql = enables_mysql(&cmd);
        for (i, m) in cmd.match_indices("--test ") {
            let name: String = cmd[i + m.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            // A target run more than once counts as covered if any one
            // of those runs builds MySQL in.
            let e = out.entry(name).or_insert(false);
            *e = *e || mysql;
        }
    }
    out
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

    let missing: Vec<&String> = on_disk.iter().filter(|s| !named.contains_key(*s)).collect();
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

    // Named is not the same as run. A step that omits the `mysql`
    // feature compiles no `tri_mysql` module and no MySQL-gated suite,
    // so the target is listed, the job is green, and the arm never
    // executes — the precise failure this guard exists to catch, one
    // level down from where it was looking.
    let unfeatured: Vec<&String> = on_disk
        .iter()
        .filter(|s| named.get(*s) == Some(&false))
        .collect();
    assert!(
        unfeatured.is_empty(),
        "these suites are named in the `mysql_live` job by a step that does not build \
         the `mysql` feature, so the MySQL arm does not exist in the binary that runs:\
         \n\n  {}\n\nAdd `mysql` to that step's `--features`, or use `--all-features`.",
        unfeatured
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
        .keys()
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
      # a comment mentioning --features mysql must not vouch for the next step
      - run: cargo test -p rustango --features sqlite --test delta_mysql_live
      - run: cargo test -p rustango --features mysqlish --test epsilon_mysql_live
      # - run: cargo test -p rustango --features mysql --test zeta_mysql_live
      - name: a step whose run: is on the next line
        run: cargo test -p rustango --features mysql --test eta_mysql_live
      - run: cargo test -p rustango --features \"mysql,testkit\" --test theta_mysql_live
      - run: cargo test -p rustango -F mysql --test iota_mysql_live
      - run: |
          echo 'a shell line that looks like a list item:'
          echo '- not a step'
          cargo test -p rustango --features mysql --test kappa_mysql_live
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
            found.contains_key("alpha_mysql_live"),
            "plain `- run:`: {found:?}"
        );
        assert!(
            found.contains_key("beta_mysql_live"),
            "`run: |` block: {found:?}"
        );
        assert!(
            found.contains_key("gamma_mysql_live"),
            "after a `\\` continuation: {found:?}"
        );
    }

    /// The feature check, which is the half that makes the name mean
    /// something.
    #[test]
    fn a_target_is_only_covered_when_its_step_builds_mysql() {
        let found = named_test_targets(&job_block(SAMPLE, "mysql_live"));

        assert_eq!(
            found.get("alpha_mysql_live"),
            Some(&true),
            "--features mysql"
        );
        assert_eq!(found.get("beta_mysql_live"), Some(&true), "mysql in a list");
        assert_eq!(found.get("gamma_mysql_live"), Some(&true), "--all-features");

        assert_eq!(
            found.get("delta_mysql_live"),
            Some(&false),
            "named by a step building only sqlite — listed, but no MySQL arm exists \
             in that binary: {found:?}"
        );
        assert_eq!(
            found.get("epsilon_mysql_live"),
            Some(&false),
            "`mysqlish` is not `mysql`; the feature must match as a whole token: \
             {found:?}"
        );
    }

    /// Spellings of `--features` that all mean "mysql is compiled in".
    ///
    /// Each of these read as *not* enabling mysql, so a correct workflow
    /// would have been reported as having a missing MySQL arm. They fail
    /// closed rather than open, which is the safer direction — but a
    /// guard that cries wolf on correct input gets ignored, which is the
    /// same end state as one that misses.
    #[test]
    fn every_spelling_of_the_feature_flag_is_understood() {
        let found = named_test_targets(&job_block(SAMPLE, "mysql_live"));
        for (target, how) in [
            ("theta_mysql_live", "a quoted feature list"),
            ("iota_mysql_live", "the short `-F` form"),
            (
                "kappa_mysql_live",
                "a `run: |` block containing a `- ` line",
            ),
        ] {
            assert_eq!(
                found.get(target),
                Some(&true),
                "{how} was not understood as enabling mysql: {found:?}"
            );
        }
    }

    /// A step written `- name:` / `run:` still counts.
    ///
    /// `mysql_live` already contains one such step. Splitting only on
    /// `- run:` missed the shape entirely, so a suite added to a named
    /// step would have been invisible to the guard — the guard's own
    /// blind spot, in the same family as the one it exists to catch.
    #[test]
    fn a_step_with_run_on_its_own_line_counts() {
        let found = named_test_targets(&job_block(SAMPLE, "mysql_live"));
        assert_eq!(
            found.get("eta_mysql_live"),
            Some(&true),
            "a `- name:` + `run:` step was not seen: {found:?}"
        );
    }

    /// A commented-out step runs nothing, so it must not count.
    ///
    /// Splitting the raw block on `- run:` matched inside
    /// `# - run: …` as well, so switching a suite off by commenting its
    /// line out left the guard green — the guard would have vouched for
    /// a step that does not exist.
    #[test]
    fn a_commented_out_step_does_not_count() {
        let found = named_test_targets(&job_block(SAMPLE, "mysql_live"));
        assert!(
            !found.contains_key("zeta_mysql_live"),
            "a commented-out `- run:` was counted as covering the suite: {found:?}"
        );
    }

    /// A comment naming `--features mysql` must not vouch for the step
    /// after it — several real comments in this job do exactly that.
    #[test]
    fn comments_do_not_leak_into_the_next_command() {
        let cmds = run_commands(&job_block(SAMPLE, "mysql_live"));
        assert!(
            cmds.iter().any(|c| c.contains("delta_mysql_live")),
            "the delta step went missing: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| c.contains("delta_mysql_live") && enables_mysql(c)),
            "the preceding comment's `--features mysql` vouched for a step that \
             builds sqlite: {cmds:?}"
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
