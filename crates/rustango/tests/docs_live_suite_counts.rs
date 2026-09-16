//! Backing test for `docs/testing.md` — the live-suite table.
//!
//! That page tells a reader which `*_live.rs` suites need a server and
//! which need nothing, as four counts. Counts rot: add a suite and the
//! table is quietly wrong, with no signal, which is the same defect the
//! page itself is warning about.
//!
//! It rotted before it shipped. The `*(none)*` row said 180, which is
//! the number of suites using `sqlite::memory:` — but the row's label is
//! "needs no environment variable", and that is 213. The other 33 use a
//! temp-file SQLite, so they need nothing either. Two measurements, one
//! published under the other's name.
//!
//! So the table is the assertion now, and this recomputes it. The docs
//! are the only copy of the numbers; nothing here restates them.
//!
//! All four translations are checked, not just the English. They are
//! four more copies of the same counts, and a table nobody can check is
//! exactly how the English one came to be wrong.

// No feature gate, deliberately. This reads files off disk and calls
// nothing from the crate, so gating it only decides whether it runs —
// and it was `#![cfg(feature = "sqlite")]`, which is not in the default
// feature set, so `cargo test -p rustango --test docs_live_suite_counts`
// reported `ok. 0 passed` while proving nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Variables a live suite reads to decide whether it can run.
///
/// `MYSQL_URL` is here as a tripwire, not as a variable anyone should
/// set. One suite read it instead of `MYSQL_TEST_URL` and so had never
/// run anywhere (#1415, fixed). Nothing reads it now, so it contributes
/// no count and needs no row — but if a suite starts reading it again,
/// it appears in the measured set with no row to match and this fails
/// naming it, which is how the original would have been caught.
const GATING_VARS: &[&str] = &[
    "DATABASE_URL",
    "MYSQL_TEST_URL",
    "MYSQL_URL",
    "REDIS_TEST_URL",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root")
}

/// `variable -> suite count`, measured from the test sources. The
/// no-variable bucket is keyed `(none)` to match the doc's row label.
fn measured(root: &Path) -> BTreeMap<String, usize> {
    let dir = root.join("crates/rustango/tests");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for entry in std::fs::read_dir(&dir).expect("read tests dir").flatten() {
        let name = entry.file_name().into_string().unwrap_or_default();
        // `_live` is the old per-dialect suffix; `_tri` is a suite that
        // runs its body on every configured backend (#1461). Both are
        // counted, or a converted file drops out of the accounting
        // entirely — which is what happened to the first one: retiring
        // three `_live` files for one `_tri` file showed up here as a
        // net loss of coverage that had not occurred.
        if !name.ends_with("_live.rs") && !name.ends_with("_tri.rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };

        // A `_tri` suite reads no variable itself — the lookup lives in
        // `Backend::pool()` — so a text scan sees nothing and files it
        // under "needs nothing, always runs". That is the worst possible
        // answer: a tri suite wants BOTH servers, and its whole purpose
        // is the two arms that do not run without them.
        //
        // Left unhandled, every conversion made the table worse in the
        // direction of its own headline: retiring a `*_mysql_live.rs`
        // for a `*_tri.rs` dropped the MySQL count by one and raised
        // `(none)` by one, so a series of PRs adding MySQL coverage
        // published a table showing MySQL coverage falling. The guard
        // passed throughout, because it measured the same wrong thing
        // the page printed.
        //
        // So they are credited to both server variables and kept out of
        // `(none)`. The SQLite arm does still run with nothing set; the
        // page says so in prose rather than in a count, because a reader
        // uses this table to decide which servers to start.
        //
        // The credit is applied *without* skipping the scan below. A
        // `continue` here took the `MYSQL_URL` tripwire out of service
        // for every tri file — that entry exists so a suite reading the
        // wrong variable name lands in the measured set with no row to
        // match and fails loudly, which is how #1415 was found. Skipping
        // the loop would let a converted suite reintroduce it unseen.
        let is_tri = name.ends_with("_tri.rs");
        let mut gated = is_tri;
        if is_tri {
            *counts.entry("DATABASE_URL".to_owned()).or_default() += 1;
            *counts.entry("MYSQL_TEST_URL".to_owned()).or_default() += 1;
        }

        // A suite is counted under every gating variable it reads; one
        // that reads none is counted as needing nothing.
        for var in GATING_VARS {
            // A tri suite was already credited to both server variables
            // above. Counting them again because the body happens to
            // mention one would double-count it, and the guard would then
            // force the docs page to publish that wrong number — a guard
            // that makes the page worse is worse than no guard.
            //
            // Keyed on `is_tri`, not on `gated`: `gated` is also set by
            // this loop, so testing it here would start skipping
            // `DATABASE_URL` for an ordinary suite as soon as any earlier
            // variable matched.
            if is_tri && matches!(*var, "DATABASE_URL" | "MYSQL_TEST_URL") {
                continue;
            }
            if text.contains(&format!("env::var(\"{var}\")")) {
                gated = true;
                // `MYSQL_URL` is counted under its own name rather than
                // folded into `MYSQL_TEST_URL`. Folding would hide the
                // fact that setting the documented variable does not run
                // that suite — which is the whole of #1415. The table
                // lists it too, so the bug is visible until it is fixed.
                *counts.entry((*var).to_owned()).or_default() += 1;
            }
        }
        if !gated {
            *counts.entry("(none)".to_owned()).or_default() += 1;
        }
    }
    counts
}

/// Every page carrying the table — the English one and its translations.
const PAGES: &[&str] = &[
    "docs/testing.md",
    "docs/de/testing.md",
    "docs/es/testing.md",
    "docs/fr/testing.md",
];

/// `variable -> count`, parsed out of the live-suite table on one page.
fn documented(root: &Path, page: &str) -> BTreeMap<String, usize> {
    let text = std::fs::read_to_string(root.join(page)).unwrap_or_else(|_| panic!("read {page}"));
    let mut out = BTreeMap::new();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 2 {
            continue;
        }
        let Ok(count) = cells[1].parse::<usize>() else {
            continue;
        };
        // `*(none)*` or `` `DATABASE_URL` `` — strip the markup, keep the name.
        let var = cells[0].trim_matches(|c| c == '*' || c == '`' || c == ' ');
        // The no-variable row is written in the page's own language —
        // `(none)`, `(keine)`, `(ninguna)`, `(aucune)`. Any parenthesised
        // label is that row; anything else must name a variable we know,
        // so an unrelated numeric table elsewhere is not mistaken for this one.
        let key = if var.starts_with('(') {
            "(none)"
        } else if GATING_VARS.contains(&var) {
            var
        } else {
            continue;
        };
        out.insert(key.to_owned(), count);
    }
    out
}

#[test]
fn the_live_suite_table_matches_the_test_tree() {
    let root = repo_root();
    let measured = measured(&root);
    let mut problems = Vec::new();

    for page in PAGES {
        let documented = documented(&root, page);

        assert!(
            !documented.is_empty(),
            "parsed no counts out of {page} — the table moved or changed shape, \
             so this guard is no longer reading what it claims to"
        );

        for (var, doc_count) in &documented {
            match measured.get(var) {
                Some(real) if real == doc_count => {}
                Some(real) => {
                    problems.push(format!("{page} — {var}: says {doc_count}, tree has {real}"))
                }
                None => problems.push(format!("{page} — {var}: says {doc_count}, tree has none")),
            }
        }
        for var in measured.keys() {
            if !documented.contains_key(var) {
                problems.push(format!(
                    "{page} — {var}: {} suite(s) read it and the table does not list it",
                    measured[var]
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "the live-suite table disagrees with the tests:\n  {}\n\n\
         A reader uses these to decide which servers to start. Update every \
         translation — these pages are the only copy of the numbers, which is \
         why they can be checked.",
        problems.join("\n  "),
    );
}

/// `testkit/matrix.rs`'s header states two counts that motivate the
/// whole harness. They are checked here for the same reason the table
/// above is: the first draft said 30 stems and 167 unpaired files
/// against a tree holding 12 and 176, and nothing noticed.
///
/// A number written once in a doc comment and never recomputed is the
/// defect this file exists to catch. It applies to the module that
/// makes the argument just as much as to the page that publishes it.
#[test]
fn the_matrix_header_counts_match_the_test_tree() {
    let root = repo_root();
    let dir = root.join("crates/rustango/tests");

    let stems: Vec<String> = std::fs::read_dir(&dir)
        .expect("read tests dir")
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            n.strip_suffix("_sqlite_live.rs").map(str::to_owned)
        })
        .collect();

    let has = |s: &str, suffix: &str| dir.join(format!("{s}{suffix}")).exists();
    let paired = stems
        .iter()
        .filter(|s| has(s, "_mysql_live.rs") || has(s, "_pg_live.rs"))
        .count();
    let unpaired = stems
        .iter()
        .filter(|s| !has(s, "_mysql_live.rs") && !has(s, "_pg_live.rs") && !has(s, "_tri.rs"))
        .count();

    let header = std::fs::read_to_string(root.join("crates/rustango/src/testkit/matrix.rs"))
        .expect("read matrix.rs");

    let claims = [
        (
            format!("Of {} `*_sqlite_live.rs` files", stems.len()),
            "total",
        ),
        // Anchored on the comma. `contains("12 stems have a sibling")`
        // is satisfied by a header saying 112, and `contains("2 …")` by
        // one saying 12 — a dropped or gained leading digit is the one
        // shape of staleness this guard exists for, and unanchored
        // `contains` is blind to exactly it.
        (format!(", {paired} stems have a sibling"), "paired stems"),
        (
            format!("**{unpaired} have no MySQL or PG counterpart"),
            "unpaired",
        ),
    ];
    let wrong: Vec<&str> = claims
        .iter()
        .filter(|(text, _)| !header.contains(text.as_str()))
        .map(|(_, what)| *what)
        .collect();

    assert!(
        wrong.is_empty(),
        "testkit/matrix.rs's header counts are stale: {}\n\nMeasured now: {} \
         `*_sqlite_live.rs` files, {paired} stems with a sibling for another \
         backend, {unpaired} with no counterpart at all.",
        wrong.join(", "),
        stems.len(),
    );
}
