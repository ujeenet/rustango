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

// =====================================================================
// Branding: the `[admin]` / `[brand]` key tables and the console's env vars
// =====================================================================
//
// English only: the translations do not carry these sections yet.

/// `pub` field names of `pub struct <name>` in `config/sections.rs`.
fn settings_fields(root: &Path, name: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(root.join("crates/rustango/src/config/sections.rs"))
        .expect("read config/sections.rs");
    let open = format!("pub struct {name} {{");
    let fields: BTreeSet<String> = text
        .lines()
        .skip_while(|l| l.trim() != open)
        .skip(1)
        .take_while(|l| l.trim() != "}")
        .filter_map(|l| l.trim().strip_prefix("pub "))
        .filter_map(|rest| rest.split(':').next())
        .map(str::to_owned)
        .collect();
    assert!(
        !fields.is_empty(),
        "parsed no fields out of `{name}` — the struct moved or changed shape"
    );
    fields
}

/// First-cell code spans of the table rows between `heading` and the next
/// heading of the same or a higher level.
fn table_keys(text: &str, heading: &str) -> BTreeSet<String> {
    let level = heading.chars().take_while(|c| *c == '#').count();
    let mut lines = text.lines().skip_while(|l| l.trim_end() != heading);
    assert!(lines.next().is_some(), "heading `{heading}` not found");
    lines
        .take_while(|l| {
            let hashes = l.chars().take_while(|c| *c == '#').count();
            !(hashes > 0 && hashes <= level && l[hashes..].starts_with(' '))
        })
        .filter_map(|l| l.trim().strip_prefix("| `"))
        .filter_map(|rest| rest.split('`').next())
        .map(str::to_owned)
        .collect()
}

fn assert_same(page: &str, what: &str, code: &BTreeSet<String>, doc: &BTreeSet<String>) {
    let missing: Vec<&String> = code.difference(doc).collect();
    let invented: Vec<&String> = doc.difference(code).collect();
    assert!(
        missing.is_empty() && invented.is_empty(),
        "{page}: the {what} table does not match the code.\n  \
         in the code but undocumented: {missing:?}\n  \
         documented but nonexistent: {invented:?}"
    );
}

/// A reader copies keys off these tables; a key the table lacks cannot
/// be found, and one only the table has is silently ignored at load.
#[test]
fn the_documented_admin_and_brand_keys_are_the_settings_fields() {
    let root = repo_root();

    let admin = std::fs::read_to_string(root.join("docs/admin.md")).expect("read admin.md");
    let documented = table_keys(&admin, "### Config: `[admin]` and `[brand]`");
    assert_same(
        "docs/admin.md",
        "`[admin]` key",
        &settings_fields(&root, "AdminSettings"),
        &documented,
    );

    let console = std::fs::read_to_string(root.join("docs/operator-console.md"))
        .expect("read operator-console.md");
    let (env, keys): (BTreeSet<String>, BTreeSet<String>) = table_keys(&console, "## Branding")
        .into_iter()
        .partition(|k| k.starts_with("RUSTANGO_"));
    assert_same(
        "docs/operator-console.md",
        "`[brand]` key",
        &settings_fields(&root, "BrandSettings"),
        &keys,
    );

    let src =
        std::fs::read_to_string(root.join("crates/rustango/src/tenancy/operator_console/mod.rs"))
            .expect("read operator_console/mod.rs");
    let read: BTreeSet<String> = src
        .split("\"RUSTANGO_OPERATOR_")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(|tail| format!("RUSTANGO_OPERATOR_{tail}"))
        .collect();
    assert!(!read.is_empty(), "found no RUSTANGO_OPERATOR_* reads");
    assert_same(
        "docs/operator-console.md",
        "`RUSTANGO_OPERATOR_*` variable",
        &read,
        &env,
    );
}
