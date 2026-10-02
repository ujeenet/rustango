//! Backing test for `docs/scaffolding.md` — the `--features` table.
//!
//! That page prints a table of the opt-in features and then says
//! **"`cargo rustango new --help` prints this list"**. That is a claim
//! about two things staying equal, and nothing was holding them to it.
//!
//! The failure is quiet in the direction that matters. Add a feature to
//! `OPTIONAL_FEATURES` and `--help` grows a row the docs don't have, so
//! the page silently becomes a subset while still asserting it is the
//! whole list — and the feature is undiscoverable to anyone reading the
//! docs rather than the binary. Remove one and the page advertises a
//! flag that is now refused at the command line.
//!
//! So the assertion runs the real binary rather than reading the
//! constant: the claim is about what `--help` *prints*, and a test that
//! read `OPTIONAL_FEATURES` directly would still pass if the printing
//! loop were changed.
//!
//! All four translations are checked. They are four more copies of the
//! same list, and a copy nobody checks is how the others drifted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root")
}

/// Feature names as `cargo rustango new --help` actually prints them.
///
/// The `FEATURES:` block is a run of indented `name  description` lines
/// ending at the next blank line.
fn help_features() -> BTreeSet<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .args(["rustango", "new", "--help"])
        .output()
        .expect("run the scaffolder");
    let text = String::from_utf8_lossy(&out.stdout);

    let mut names = BTreeSet::new();
    let mut in_features = false;
    for line in text.lines() {
        if line.trim_end() == "FEATURES:" {
            in_features = true;
            continue;
        }
        if in_features {
            if line.trim().is_empty() {
                break;
            }
            if let Some(name) = line.split_whitespace().next() {
                names.insert(name.to_owned());
            }
        }
    }
    assert!(
        !names.is_empty(),
        "parsed no features out of `new --help` — the FEATURES: block moved \
         or changed shape, so this guard is no longer reading what it claims to"
    );
    names
}

/// Feature names listed in the `--features` table on one page.
///
/// Markdown tables are runs of `|` lines; the wanted one is identified
/// by a row naming `email-smtp`, since the feature names are the same
/// identifiers in every language while the prose around them is not.
fn documented_features(page_text: &str) -> BTreeSet<String> {
    let mut table: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for line in page_text.lines() {
        let line = line.trim();
        if line.starts_with('|') {
            // Only the first cell — the later ones mention features in
            // prose ("OIDC single sign-on … for the admin site").
            let first = line.trim_matches('|').split('|').next().unwrap_or("");
            current.extend(
                first
                    .split('`')
                    .skip(1)
                    .step_by(2)
                    .map(std::borrow::ToOwned::to_owned),
            );
        } else if !current.is_empty() {
            if current.iter().any(|t| t == "email-smtp") {
                table = std::mem::take(&mut current);
            } else {
                current.clear();
            }
        }
    }
    if table.is_empty() && current.iter().any(|t| t == "email-smtp") {
        table = current;
    }
    table.into_iter().collect()
}

#[test]
fn the_documented_feature_table_is_the_one_help_prints() {
    let root = repo_root();
    let printed = help_features();

    for page in [
        "docs/scaffolding.md",
        "docs/de/scaffolding.md",
        "docs/es/scaffolding.md",
        "docs/fr/scaffolding.md",
    ] {
        let text = std::fs::read_to_string(root.join(page)).expect("read page");
        let documented = documented_features(&text);
        assert!(
            !documented.is_empty(),
            "{page}: found no `--features` table — it moved or changed shape"
        );
        assert_eq!(
            documented, printed,
            "{page} does not list the features `cargo rustango new --help` prints. \
             The page says it prints this list, so a reader takes it as complete: \
             a name only `--help` has is undiscoverable from the docs, and a name \
             only the page has is refused at the command line."
        );
    }
}
