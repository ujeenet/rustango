//! Every published page must be backed by a test, and the untested backlog
//! may only shrink.
//!
//! A docs audit in September 2026 found 107 places where a page and the code
//! it describes disagreed. The four worst pages — `manage.md`,
//! `getting-started.md`, `orm.md`, `security.md`, 37 findings between them —
//! all had no backing test.
//!
//! Don't read that as proof that tests prevent defects. It is confounded at
//! least three ways: the biggest pages attract both more findings and less
//! coverage, those two are also the oldest and most-edited pages, and the
//! reviewers could see which pages had tests. It points where to spend effort,
//! which is the same direction "the biggest pages are the worst" points.
//!
//! The argument for this test doesn't rest on the correlation anyway. Prose is
//! reviewed by reading it, and
//! reading is exactly the check that a confidently-worded wrong sentence
//! passes. `fetch_pool()` sat in the README's headline example for two
//! releases after the rename; `.execute(&pool)` appears eleven times in
//! `orm.md` and has never existed. Both read fine.
//!
//! So this is a test instead. It enforces two things:
//!
//! 1. **Coverage.** A published page has a backing test, or an entry in
//!    [`UNBACKED`] recording that it doesn't. Adding a page with neither
//!    fails. This is a ratchet: a page listed in `UNBACKED` that *has*
//!    acquired a test also fails, so the list cannot silently go stale.
//!
//! 2. **A fence budget.** A page in `UNBACKED` may not gain Rust examples.
//!    The recorded number is what it had when the backlog was taken; adding
//!    a twelfth untested snippet to a page with eleven fails. New examples
//!    go on pages that compile them, or they come with a backing test.
//!
//! ## Declaring a backing test
//!
//! Put this in the test's module docs — the convention predates this file and
//! roughly twenty tests already follow it:
//!
//! ```text
//! //! Backing test for `docs/auth-jwt.md` — standalone HS256 JWTs.
//! ```
//!
//! The page name is what links them. A test may declare more than one page,
//! and a page may be declared by more than one test.
//!
//! ## Paying the backlog down
//!
//! Write a test that exercises the page's examples against the real API,
//! declare it with the header above, then delete that page's `UNBACKED` row.
//! This test will tell you if you delete the wrong one.
//!
//! Prefer a compile gate where the examples are illustrative rather than
//! runnable — see `examples/getting_started_blog/tests/guide_crud_compiles.rs`,
//! which is wired into the `sqlite_litmus` CI job and so also catches a
//! Postgres-only method presented as portable.
//!
//! Run: `cargo test -p rustango --test docs_contract`

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Published pages with no backing test, and the number of Rust fences each
/// had when the backlog was recorded (2026-09-13, workspace 0.57.0).
///
/// **This list may only shrink.** Entries are the documentation debt: 157
/// Rust examples that nothing compiles or runs. Ordered worst-first.
const UNBACKED: &[(&str, usize)] = &[
    ("orm.md", 57),
    ("security.md", 32),
    ("serializers.md", 15),
    ("getting-started.md", 12),
    ("urls.md", 10),
    ("admin.md", 9),
    ("manage.md", 8),
    ("api-conventions.md", 5),
    ("query-method.md", 4),
    ("websockets.md", 3),
    ("sso.md", 2),
    ("operator-console.md", 1),
    ("scaffolding.md", 1),
    ("benchmarks.md", 0),
    ("database-tuning.md", 0),
    ("glossary.md", 0),
    ("migrations.md", 0),
];

fn repo_root() -> PathBuf {
    // crates/rustango/tests/ -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Pages listed in `docs/index.toml`, i.e. the ones readers actually see.
fn published_pages(root: &Path) -> BTreeSet<String> {
    let toml =
        std::fs::read_to_string(root.join("docs/index.toml")).expect("docs/index.toml is readable");
    let mut pages = BTreeSet::new();
    let mut in_list = false;

    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with("pages") {
            in_list = true;
        }
        if in_list {
            let mut rest = line;
            while let Some(open) = rest.find('"') {
                let after = &rest[open + 1..];
                let Some(close) = after.find('"') else { break };
                let value = &after[..close];
                if value.ends_with(".md") {
                    pages.insert(value.to_string());
                }
                rest = &after[close + 1..];
            }
            if line.contains(']') {
                in_list = false;
            }
        }
    }

    assert!(!pages.is_empty(), "parsed no pages out of docs/index.toml");
    pages
}

/// Walk every `.rs` under `crates/`, skipping build output.
fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name == "target" || name == "node_modules" || name == ".git" {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// page name -> the tests declaring `Backing test for `docs/<page>``.
fn backing_tests(root: &Path) -> BTreeMap<String, Vec<String>> {
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);

    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // The declaration lives in the module header; don't scan whole files.
        let head: String = text.lines().take(40).collect::<Vec<_>>().join("\n");
        if !head.contains("acking test for") {
            continue;
        }
        let rel = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();

        let mut rest = head.as_str();
        while let Some(at) = rest.find("docs/") {
            let after = &rest[at + 5..];
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
                .unwrap_or(after.len());
            let candidate = &after[..end];
            if candidate.ends_with(".md") {
                map.entry(candidate.to_string())
                    .or_default()
                    .push(rel.clone());
            }
            rest = &after[end..];
        }
    }
    map
}

/// Count ```rust / ```rs fences, ignoring anything inside a fence.
fn rust_fences(markdown: &str) -> usize {
    let mut count = 0;
    let mut inside = false;

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            continue;
        };
        if inside {
            inside = false;
            continue;
        }
        inside = true;
        let lang = rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .next()
            .unwrap_or("");
        if lang == "rust" || lang == "rs" {
            count += 1;
        }
    }
    count
}

#[test]
fn every_published_page_is_backed_or_declared_unbacked() {
    let root = repo_root();
    let published = published_pages(&root);
    let backed = backing_tests(&root);
    let declared: BTreeMap<&str, usize> = UNBACKED.iter().copied().collect();

    let mut undeclared = Vec::new();
    for page in &published {
        if backed.contains_key(page.as_str()) || declared.contains_key(page.as_str()) {
            continue;
        }
        undeclared.push(page.clone());
    }

    let rows: Vec<String> = undeclared
        .iter()
        .map(|page| {
            let fences = std::fs::read_to_string(root.join("docs").join(page))
                .map(|t| rust_fences(&t))
                .unwrap_or(0);
            format!("    (\"{page}\", {fences}),")
        })
        .collect();

    assert!(
        undeclared.is_empty(),
        "these published pages have no backing test and are not in UNBACKED:\n  {}\n\n\
         THIS IS EXPECTED IF YOU JUST ADDED A PAGE. Publishing a page is a \
         decision this test makes you state, not a bug in the test.\n\n\
         Pick one:\n\n\
         (a) Back it with a test — preferred. Put this in the test's module header:\n\
         \x20      //! Backing test for `docs/<page>`\n\n\
         (b) Declare the debt. Paste into UNBACKED in this file:\n\n{}\n\n\
         A zero-fence page (pure prose, links, tables) is a fine (b) — the row \
         costs nothing and makes the fence budget meaningful, so if anyone later \
         adds a Rust example to it the ratchet catches that.",
        undeclared.join("\n  "),
        rows.join("\n"),
    );
}

#[test]
fn unbacked_list_does_not_go_stale() {
    let root = repo_root();
    let backed = backing_tests(&root);
    let published = published_pages(&root);

    let mut now_backed = Vec::new();
    let mut unpublished = Vec::new();

    for (page, _) in UNBACKED {
        if let Some(tests) = backed.get(*page) {
            now_backed.push(format!("{page} — backed by {}", tests.join(", ")));
        }
        if !published.contains(*page) {
            unpublished.push(*page);
        }
    }

    assert!(
        now_backed.is_empty(),
        "these pages have a backing test but are still listed in UNBACKED:\n  {}\n\n\
         Delete their rows — the backlog is meant to shrink, and a stale entry \
         hides the win.",
        now_backed.join("\n  "),
    );

    assert!(
        unpublished.is_empty(),
        "UNBACKED lists pages that docs/index.toml does not publish: {unpublished:?}\n\
         Remove them; unpublished pages are not part of this contract.",
    );
}

#[test]
fn unbacked_pages_gain_no_new_examples() {
    let root = repo_root();
    let mut over_budget = Vec::new();

    for (page, budget) in UNBACKED {
        let path = root.join("docs").join(page);
        let Ok(text) = std::fs::read_to_string(&path) else {
            panic!("UNBACKED names a page that does not exist: docs/{page}");
        };
        let actual = rust_fences(&text);
        if actual > *budget {
            over_budget.push(format!(
                "docs/{page}: {actual} Rust fences, budget {budget} (+{})",
                actual - budget
            ));
        }
    }

    assert!(
        over_budget.is_empty(),
        "untested pages gained Rust examples:\n  {}\n\n\
         A page with no backing test may not accumulate more unverified code. \
         Either write the backing test and remove the page from UNBACKED, or \
         put the example on a page that compiles its snippets.\n\
         Lowering a budget after deleting examples is fine — raising one is the \
         thing this test exists to stop.",
        over_budget.join("\n  "),
    );
}

#[test]
fn budgets_match_reality_when_lower() {
    // If a page has fewer fences than its budget, the budget is stale and
    // should be tightened — otherwise the ratchet has slack in it.
    let root = repo_root();
    let mut slack = Vec::new();

    for (page, budget) in UNBACKED {
        let path = root.join("docs").join(page);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let actual = rust_fences(&text);
        if actual < *budget {
            slack.push(format!(
                "docs/{page}: {actual} fences, budget still {budget}"
            ));
        }
    }

    assert!(
        slack.is_empty(),
        "these budgets are looser than the page needs:\n  {}\n\n\
         Tighten them to the current count so the ratchet keeps its grip.",
        slack.join("\n  "),
    );
}
