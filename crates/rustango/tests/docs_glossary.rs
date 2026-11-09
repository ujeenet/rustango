//! Every term a page's "New to a term here?" box names must have a
//! glossary entry.
//!
//! The boxes send beginners to `glossary.md` for words like *session* or
//! *HMAC*. When the entry is missing, the reader lands on a page that does
//! not explain the word. Applies to the English pages; a glossary entry is
//! a line that starts with `**Term**`.
//!
//! Run: `cargo test -p rustango --test docs_glossary`

use std::path::{Path, PathBuf};

const MARKER: &str = "> **New to a term here?**";

fn docs_dir() -> PathBuf {
    // crates/rustango/tests/ -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .join("docs")
}

/// Lowercased names of the glossary entries: the bold text at line start.
fn entry_names(glossary: &str) -> Vec<String> {
    glossary
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("**")?;
            let end = rest.find("**")?;
            (end > 0).then(|| rest[..end].to_lowercase())
        })
        .collect()
}

/// The text of the box: the marker line plus the quoted lines after it, up
/// to a blank quote line or the end of the quote.
fn glossary_box(page: &str) -> Option<String> {
    let mut lines = page.lines();
    let first = lines.find(|l| l.starts_with(MARKER))?;
    let mut text = first[MARKER.len()..].to_owned();
    for line in lines {
        match line.strip_prefix('>') {
            Some(rest) if !rest.trim().is_empty() => {
                text.push(' ');
                text.push_str(rest);
            }
            _ => break,
        }
    }
    Some(text)
}

/// The `*italic*` spans of a box; `**bold**` is skipped.
fn italic_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('*') {
        let after = &rest[start + 1..];
        if let Some(bold) = after.strip_prefix('*') {
            rest = bold;
            continue;
        }
        let Some(end) = after.find('*') else { break };
        out.push(after[..end].trim().to_lowercase());
        rest = &after[end + 1..];
    }
    out
}

/// `needle` occurs in `hay` at the start of a word.
fn at_word_start(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
    })
}

/// A term is covered when an entry name contains it at a word start. A
/// `a/b` term is also covered when each part is.
fn covered(term: &str, names: &[String]) -> bool {
    let has = |t: &str| names.iter().any(|n| at_word_start(n, t));
    has(term)
        || (term.contains('/')
            && term
                .split('/')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .all(has))
}

#[test]
fn every_glossary_box_term_has_an_entry() {
    let docs = docs_dir();
    let glossary = std::fs::read_to_string(docs.join("glossary.md")).expect("read glossary.md");
    let names = entry_names(&glossary);
    assert!(
        names.len() > 50,
        "parsed only {} glossary entries",
        names.len()
    );

    let mut boxes = 0;
    let mut missing = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&docs)
        .expect("read docs/")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    entries.sort();
    for path in entries {
        let page = path.file_name().expect("file name").to_string_lossy();
        if page == "glossary.md" {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read page");
        let Some(found) = glossary_box(&text) else {
            continue;
        };
        boxes += 1;
        let terms = italic_terms(&found);
        assert!(!terms.is_empty(), "{page}: glossary box names no *term*");
        for term in terms {
            if !covered(&term, &names) {
                missing.push(format!("{page}: *{term}*"));
            }
        }
    }

    // About twenty-five pages carry a box; a parser that finds none proves nothing.
    assert!(boxes >= 20, "found only {boxes} glossary boxes");
    assert!(
        missing.is_empty(),
        "{} term(s) named in a glossary box have no entry in docs/glossary.md:\n  {}\n\n\
         Add a `**Term** — one plain sentence.` entry, or drop the term from the box.",
        missing.len(),
        missing.join("\n  "),
    );
}

#[test]
fn italic_terms_skip_bold() {
    assert_eq!(
        italic_terms(" *hash*, **Rustango** and *access/refresh token* — see"),
        vec!["hash".to_owned(), "access/refresh token".to_owned()],
    );
}

#[test]
fn slash_terms_need_every_part() {
    let names = vec!["access token / refresh token".to_owned()];
    assert!(covered("access/refresh token", &names));
    assert!(!covered("access/rotation", &names));
    assert!(!covered("token rotation", &names));
}
