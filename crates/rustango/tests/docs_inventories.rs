//! Backing test for `docs/testing.md` — the assertion-helper inventory.
//!
//! Some doc pages publish a *list the code owns*: "also available:
//! …", a table of features, a set of context keys. Those read as
//! complete, and a reader treats them as complete — so the failure
//! when one falls behind is not that the page is wrong, it is that a
//! helper exists and nobody can find it.
//!
//! This file is for that class. One assertion per published list,
//! rather than a new test file per sentence.
//!
//! Nothing here is wrong today. It is here so that stays true.

// No feature gate, deliberately. This reads files off disk and calls
// nothing from the crate, so gating it only decides whether it runs —
// and it was `#![cfg(feature = "sqlite")]`, which is not in the default
// feature set, so `cargo test -p rustango --test docs_inventories`
// reported `ok. 0 passed` while proving nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Pages carrying the inventory — the English one and its translations.
const TESTING_PAGES: &[&str] = &[
    "docs/testing.md",
    "docs/de/testing.md",
    "docs/es/testing.md",
    "docs/fr/testing.md",
];

/// std macros that appear in the pages' Rust samples. They are not
/// helpers and never were.
///
/// The `assert_` prefix does the rest of the work: it keeps out
/// camelCase `assertFoo` helper names a page may quote from elsewhere,
/// and the English words "assertion" and "assertions".
const STD_MACROS: &[&str] = &["assert_eq", "assert_ne"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root")
}

/// Every helper `rustango::test_assertions` exports.
fn exported_helpers(root: &Path) -> BTreeSet<String> {
    let src = root.join("crates/rustango/src/test_assertions.rs");
    let text = std::fs::read_to_string(src).expect("read test_assertions.rs");

    let names: BTreeSet<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub fn "))
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|n| n.starts_with("assert_"))
        .collect();

    assert!(
        !names.is_empty(),
        "parsed no `pub fn assert_*` out of test_assertions.rs — the module \
         moved or changed shape, so this guard is no longer reading what it \
         claims to"
    );
    names
}

/// Helper names a page mentions.
///
/// Every `assert_*` token on the page, minus the std macros. Scoping by
/// token rather than by section heading is what lets one rule read all
/// four translations: the helper names are the same identifiers in
/// every language while the headings around them are not.
fn mentioned_helpers(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        if !bytes[i].is_ascii_alphabetic() || (i > 0 && is_ident(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident(bytes[i]) {
            i += 1;
        }
        let word: String = bytes[start..i].iter().collect();
        if word.starts_with("assert_") && !STD_MACROS.contains(&word.as_str()) {
            out.insert(word);
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[test]
fn the_documented_assertion_helpers_are_the_ones_that_exist() {
    let root = repo_root();
    let exported = exported_helpers(&root);

    for page in TESTING_PAGES {
        let text = std::fs::read_to_string(root.join(page)).expect("read page");
        let mentioned = mentioned_helpers(&text);

        let missing: Vec<&String> = exported.difference(&mentioned).collect();
        let invented: Vec<&String> = mentioned.difference(&exported).collect();

        assert!(
            missing.is_empty() && invented.is_empty(),
            "{page} does not list the helpers `test_assertions` exports.\n  \
             exported but undocumented: {missing:?}\n  \
             documented but nonexistent: {invented:?}\n\n\
             The page presents this as the available set, so a helper missing \
             from it exists and cannot be found, and one only the page has \
             does not compile."
        );
    }
}

// =====================================================================
// `docs/logging.md` — the tracing-target table
// =====================================================================

/// Pages carrying the target table.
const LOGGING_PAGES: &[&str] = &[
    "docs/logging.md",
    "docs/de/logging.md",
    "docs/es/logging.md",
    "docs/fr/logging.md",
];

/// Every target the framework names explicitly in a `target:` argument.
///
/// Events that pass no `target:` inherit their module path and are
/// reachable the same way — the page says so, and they are deliberately
/// out of this set, because there is no list of them to drift against.
fn explicit_targets(root: &Path) -> BTreeSet<String> {
    let src = root.join("crates/rustango/src");
    let mut out = BTreeSet::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for line in text.lines() {
                    let mut rest = line;
                    while let Some(at) = rest.find("target: \"rustango::") {
                        let tail = &rest[at + "target: \"".len()..];
                        if let Some(end) = tail.find('"') {
                            out.insert(tail[..end].to_owned());
                            rest = &tail[end..];
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    }
    assert!(
        !out.is_empty(),
        "found no `target: \"rustango::…\"` sites — the convention changed, \
         so this guard is no longer reading what it claims to"
    );
    out
}

/// Targets a page's table lists, as `| `rustango::x` | …` rows.
fn documented_targets(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("| `rustango::"))
        .filter_map(|rest| rest.split('`').next())
        .map(|name| format!("rustango::{name}"))
        .collect()
}

/// The page presents the table as *the* filter vocabulary: a reader picks
/// `RUST_LOG=rustango::tenancy=debug` off it. A target missing from the
/// table is a subsystem nobody can turn up, and one only the table has is
/// a filter that matches nothing and looks like silence.
#[test]
fn the_documented_tracing_targets_are_the_ones_that_exist() {
    let root = repo_root();
    let actual = explicit_targets(&root);

    for page in LOGGING_PAGES {
        let path = root.join(page);
        if !path.exists() {
            continue; // translation not landed yet
        }
        let text = std::fs::read_to_string(&path).expect("read page");
        let documented = documented_targets(&text);

        let missing: Vec<&String> = actual.difference(&documented).collect();
        let invented: Vec<&String> = documented.difference(&actual).collect();

        assert!(
            missing.is_empty() && invented.is_empty(),
            "{page}'s target table does not match the code.\n  \
             emitted but undocumented: {missing:?}\n  \
             documented but never emitted: {invented:?}"
        );
    }
}
