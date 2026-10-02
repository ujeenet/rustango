//! A doc comment must document the item directly beneath it.
//!
//! Rust attaches `///` to the *next* item, with no blank line
//! required. So inserting a new item immediately below an existing
//! doc block silently re-parents that block: the new item publishes
//! someone else's documentation, and the original is left bare. It is
//! invisible in review, because the diff shows only added lines.
//!
//! This has now happened three times in one release:
//!
//! * #1503 — `Format` was inserted under `Setup`'s doc, so `Format`'s
//!   page opened "Builder for the tracing-subscriber config" with a
//!   `Self::install` link resolving nowhere, and `Setup` — the public
//!   entry point — had no page at all.
//! * `forms::csrf` — `split_origin` took `origin_allowed`'s entire
//!   Origin policy, including bullets added in the same commit.
//! * `tenancy::agents` — `AgentAuthState` took
//!   `agent_token_still_valid_pool`'s body *and* its `# Errors`
//!   section, both of which docs.rs would have published on a struct
//!   that returns nothing.
//!
//! Three instances is a pattern, not luck, so it gets a guard.
//!
//! ## What this detects
//!
//! A doc block whose **last line** reads like the tail of a function
//! contract — an `# Errors`, `# Panics` or `# Safety` section — sitting
//! directly above an item that cannot have one: a `struct`, `enum`,
//! `const`, `static` or `type`. That is the unambiguous shape. A
//! prose-only block re-parented onto a struct is not distinguishable
//! from a struct's own documentation by reading alone, so this does
//! not try.

use std::path::{Path, PathBuf};

/// Section headings that only a function or method can honour.
const FUNCTION_ONLY: &[&str] = &["# Errors", "# Panics", "# Safety"];

/// Item keywords that cannot return, panic, or be unsafe to call.
const NON_CALLABLE: &[&str] = &[
    "struct ", "enum ", "const ", "static ", "type ", "union ", "trait ",
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Strip `pub`, `pub(crate)`, `async`, `unsafe` and attributes so the
/// item keyword is first.
fn item_keyword(line: &str) -> String {
    let mut s = line.trim();
    loop {
        let next = s
            .strip_prefix("pub(crate) ")
            .or_else(|| s.strip_prefix("pub(super) "))
            .or_else(|| s.strip_prefix("pub "))
            .or_else(|| s.strip_prefix("async "))
            .or_else(|| s.strip_prefix("unsafe "))
            .or_else(|| s.strip_prefix("extern "));
        match next {
            Some(rest) => s = rest,
            None => break,
        }
    }
    s.to_owned()
}

#[test]
fn a_function_contract_never_documents_a_struct() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&root, &mut files);
    assert!(
        files.len() > 50,
        "expected the whole src tree, saw {} files — the walk is broken \
         and this guard would pass vacuously",
        files.len(),
    );

    let mut offences = Vec::new();
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<&str> = src.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            // A doc line that is a function-only section heading.
            let is_heading = t
                .strip_prefix("///")
                .map(str::trim)
                .is_some_and(|h| FUNCTION_ONLY.contains(&h));
            if !is_heading {
                continue;
            }
            // Walk forward past the rest of the doc block and any
            // attributes to the item it attaches to.
            let mut j = i + 1;
            while j < lines.len() {
                let n = lines[j].trim();
                if n.starts_with("///") || n.starts_with("#[") || n.is_empty() {
                    j += 1;
                    continue;
                }
                break;
            }
            let Some(item) = lines.get(j) else { continue };
            let kw = item_keyword(item);
            // `const fn` and `unsafe fn` are functions; only the
            // data-carrying `const`/`static` forms are not.
            if kw.starts_with("const fn ") || kw.starts_with("fn ") {
                continue;
            }
            if NON_CALLABLE.iter().any(|k| kw.starts_with(k)) {
                let rel = path
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(path)
                    .display();
                offences.push(format!(
                    "{rel}:{}: `{}` documents `{}`",
                    i + 1,
                    t,
                    kw.trim()
                ));
            }
        }
    }

    assert!(
        offences.is_empty(),
        "a `{}` section is attached to an item that cannot have one, which \
         means a doc block was re-parented by an item inserted beneath it. \
         The item below now publishes another item's contract, and that \
         other item is undocumented:\n  {}\n\nAdd a blank line, or move the \
         block back onto the function it describes.",
        FUNCTION_ONLY.join("` / `"),
        offences.join("\n  "),
    );
}
