//! The CI dedupe in `ci.yml` is wired to every job.
//!
//! `already_green` skips a run whose tree already passed, and
//! `record_green` marks a tree as passed. A job missing from
//! `record_green`'s `needs:` lets a tree be recorded green without it; a
//! job that does not reach `already_green` runs on every deduped run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
}

const SKIP_TEST: &str = "needs.already_green.outputs.skip != 'true'";

#[derive(Default, Debug)]
struct Job {
    needs: Vec<String>,
    /// The job-level `if:`, folded onto one line.
    cond: Option<String>,
}

/// Job name for a header line: a key at exactly two spaces of indent.
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
    (ok && rest.contains(':')).then_some(name)
}

fn strip_comment(line: &str) -> &str {
    match line.find(" #") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// `needs:` and `if:` of every job, in any of the YAML shapes ci.yml uses.
fn parse(yaml: &str) -> BTreeMap<String, Job> {
    let at = yaml.find("\njobs:\n").expect("no top-level `jobs:` key");
    let mut jobs: BTreeMap<String, Job> = BTreeMap::new();
    let mut current: Option<String> = None;
    // The job-level key whose value is continued on deeper lines.
    let mut open: Option<&'static str> = None;
    for raw in yaml[at + "\njobs:\n".len()..].lines() {
        if raw.trim_start().starts_with('#') || raw.trim().is_empty() {
            continue;
        }
        if let Some(name) = job_name(raw) {
            current = Some(name.to_owned());
            jobs.entry(name.to_owned()).or_default();
            open = None;
            continue;
        }
        let Some(job) = current.as_ref().and_then(|n| jobs.get_mut(n)) else {
            continue;
        };
        let line = strip_comment(raw).trim_end();
        let text = line.trim();
        if indent(line) == 4 {
            open = None;
            if let Some(v) = text.strip_prefix("needs:") {
                let v = v.trim();
                if v.is_empty() {
                    open = Some("needs");
                } else {
                    job.needs.extend(
                        v.trim_start_matches('[')
                            .trim_end_matches(']')
                            .split(',')
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty()),
                    );
                }
            } else if let Some(v) = text.strip_prefix("if:") {
                job.cond = Some(v.trim().trim_start_matches(">-").trim().to_owned());
                open = Some("if");
            }
        } else if indent(line) > 4 {
            match open {
                Some("needs") => {
                    if let Some(item) = text.strip_prefix("- ") {
                        job.needs.push(item.trim().to_owned());
                    }
                }
                Some("if") => {
                    let c = job.cond.get_or_insert_with(String::new);
                    c.push(' ');
                    c.push_str(text);
                }
                _ => {}
            }
        }
    }
    jobs
}

fn problems(yaml: &str) -> Vec<String> {
    let jobs = parse(yaml);
    let mut out = Vec::new();
    let Some(first) = jobs.get("already_green") else {
        return vec!["no `already_green` job".into()];
    };
    if !first.needs.is_empty() || first.cond.is_some() {
        out.push("`already_green` must have no `needs:` or `if:`".into());
    }
    let Some(last) = jobs.get("record_green") else {
        return vec!["no `record_green` job".into()];
    };
    // Its default `if:` is what refuses a run with skipped jobs.
    if last.cond.is_some() {
        out.push("`record_green` has an `if:`; it would record skipped jobs as green".into());
    }
    let listed: BTreeSet<&str> = last.needs.iter().map(String::as_str).collect();
    for name in jobs.keys().filter(|n| *n != "record_green") {
        if !listed.contains(name.as_str()) {
            out.push(format!("`record_green` does not need `{name}`"));
        }
    }

    for (name, job) in &jobs {
        if name == "already_green" {
            continue;
        }
        // A skipped ancestor skips the job only under the default status check.
        if let Some(c) = &job.cond {
            if ["always()", "cancelled()", "failure()"]
                .iter()
                .any(|f| c.contains(f))
            {
                out.push(format!("`{name}` has a status function in `if:`: {c}"));
            }
        }
        if job.needs.iter().any(|n| n == "already_green")
            && name != "record_green"
            && !job.cond.as_deref().is_some_and(|c| c.contains(SKIP_TEST))
        {
            out.push(format!(
                "`{name}` needs `already_green` but its `if:` lacks `{SKIP_TEST}`"
            ));
        }
        let mut seen = BTreeSet::new();
        let mut todo: Vec<&str> = job.needs.iter().map(String::as_str).collect();
        let mut reaches = false;
        while let Some(n) = todo.pop() {
            if n == "already_green" {
                reaches = true;
                break;
            }
            if seen.insert(n) {
                if let Some(j) = jobs.get(n) {
                    todo.extend(j.needs.iter().map(String::as_str));
                }
            }
        }
        if !reaches {
            out.push(format!(
                "`{name}` does not reach `already_green` through `needs:`"
            ));
        }
    }
    out
}

/// Manual and weekly runs must never skip: the weekly one is what still
/// saves develop's build cache once dedupe skips its pushes.
fn never_skip_problems(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let on = yaml
        .find("\non:\n")
        .map(|at| &yaml[at + 1..])
        .and_then(|rest| rest.find("\njobs:\n").map(|end| &rest[..end]))
        .unwrap_or("");
    if !on.contains("\n  schedule:\n") {
        out.push("`on:` has no `schedule:` trigger".into());
    }
    let job = yaml
        .find("\n  already_green:\n")
        .map(|at| &yaml[at + 1..])
        .unwrap_or("");
    let job = &job[..job.find("\n  gate:\n").unwrap_or(job.len())];
    let expr = job
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("NEVER_SKIP:"))
        .unwrap_or("");
    for event in ["workflow_dispatch", "schedule"] {
        if !expr.contains(&format!("github.event_name == '{event}'")) {
            out.push(format!(
                "`already_green`'s NEVER_SKIP does not cover `{event}`"
            ));
        }
    }
    if !job.contains(r#"if [ "$NEVER_SKIP" = true ]; then"#) {
        out.push("`already_green` does not act on NEVER_SKIP".into());
    }
    out
}

#[test]
fn manual_and_weekly_runs_never_skip() {
    let yaml = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .expect("ci.yml is readable");
    let found = never_skip_problems(&yaml);
    assert!(
        found.is_empty(),
        "ci.yml never-skip events:\n  {}",
        found.join("\n  ")
    );
}

#[test]
fn every_ci_job_is_deduped_and_recorded() {
    let yaml = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .expect("ci.yml is readable");
    let found = problems(&yaml);
    assert!(
        found.is_empty(),
        "ci.yml dedupe wiring:\n  {}",
        found.join("\n  ")
    );
}

mod parser_tests {
    use super::*;

    const GOOD: &str = "
on: push
jobs:
  already_green:
    runs-on: x
  gate:
    needs: already_green
    if: >-
      needs.already_green.outputs.skip != 'true'
      && github.event_name != 'pull_request'
  fmt:
    needs: already_green
    if: needs.already_green.outputs.skip != 'true'
  live-a:
    needs: [gate, fmt]
  record_green:
    needs:
      - already_green
      - gate
      - fmt
      - live-a
";

    #[test]
    fn a_sound_wiring_passes() {
        assert_eq!(problems(GOOD), Vec::<String>::new());
    }

    #[test]
    fn every_way_to_break_it_is_caught() {
        let cases = [
            // A new job with no `needs:` runs on every deduped run.
            GOOD.replace("  record_green:", "  new:\n    runs-on: x\n  record_green:"),
            // Left out of `record_green`.
            GOOD.replace("      - live-a\n", ""),
            // Needs the dedupe but ignores its answer.
            GOOD.replace("    if: needs.already_green.outputs.skip != 'true'\n", ""),
            // Runs past a skipped ancestor.
            GOOD.replace(
                "    needs: [gate, fmt]\n",
                "    needs: [gate, fmt]\n    if: always()\n",
            ),
            // Would record a partial run.
            GOOD.replace("  record_green:\n", "  record_green:\n    if: always()\n"),
        ];
        for bad in cases {
            assert!(!problems(&bad).is_empty(), "should fail:\n{bad}");
        }
    }

    const NEVER: &str = "
on:
  workflow_dispatch:
  schedule:
    - cron: \"0 0 * * 1\"
jobs:
  already_green:
    steps:
      - env:
          NEVER_SKIP: ${{ github.event_name == 'workflow_dispatch' || github.event_name == 'schedule' }}
        run: |
          if [ \"$NEVER_SKIP\" = true ]; then
  gate:
    runs-on: x
";

    #[test]
    fn a_dropped_never_skip_event_is_caught() {
        assert_eq!(never_skip_problems(NEVER), Vec::<String>::new());
        for bad in [
            NEVER.replace("  schedule:\n    - cron: \"0 0 * * 1\"\n", ""),
            NEVER.replace(" || github.event_name == 'schedule'", ""),
            NEVER.replace("github.event_name == 'workflow_dispatch' || ", ""),
            NEVER.replace("if [ \"$NEVER_SKIP\" = true ]; then", "true"),
        ] {
            assert!(!never_skip_problems(&bad).is_empty(), "should fail:\n{bad}");
        }
    }
}
