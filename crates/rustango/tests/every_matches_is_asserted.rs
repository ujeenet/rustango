//! A `matches!(..);` statement throws its `bool` away, so the test it
//! sits in checks nothing. Seven tests did this and passed whatever the
//! code returned. Every `matches!` must feed an `assert!` or a value.

use std::fs;
use std::path::Path;

#[path = "support/rust_files.rs"]
mod rust_files;
use rust_files::rust_files;

/// Line numbers of every `matches!(..)` at statement position whose
/// value is dropped by a trailing `;`.
fn bare_matches(src: &str) -> Vec<usize> {
    // Whole-line comments out, so a commented `;` is not a statement end.
    let code: String = src
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    for (at, _) in code.match_indices("matches!(") {
        // `assert_matches!(`, `std::matches!(` are other macros or paths.
        if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || b"_:".contains(&bytes[at - 1])) {
            continue;
        }
        let prev = code[..at].trim_end().bytes().last();
        if !matches!(prev, None | Some(b';' | b'{' | b'}')) {
            continue;
        }
        let Some(close) = closing_paren(&code, at + "matches!".len()) else {
            continue;
        };
        if code[close + 1..].trim_start().starts_with(';') {
            out.push(code[..at].matches('\n').count() + 1);
        }
    }
    out
}

/// Index of the `)` closing the `(` at `open`, skipping string and
/// char literals.
fn closing_paren(code: &str, open: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                // `'\''` or `'('`; a lifetime has no closing quote.
                b'\'' if bytes.get(i + 1) == Some(&b'\\') => {
                    i = code
                        .get(i + 3..)
                        .and_then(|r| r.find('\''))
                        .map_or(i, |q| i + 3 + q);
                }
                b'\'' => {
                    let len = code[i + 1..].chars().next().map_or(1, char::len_utf8);
                    if bytes.get(i + 1 + len) == Some(&b'\'') {
                        i += 1 + len;
                    }
                }
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[test]
fn no_matches_statement_is_discarded() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    assert!(files.len() > 500, "walked only {} files", files.len());

    let mut offenders = Vec::new();
    for path in &files {
        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        // Its own self-test snippets are the one expected hit.
        if path.ends_with("every_matches_is_asserted.rs") {
            continue;
        }
        for line in bare_matches(&src) {
            let rel = path.strip_prefix(&crates).unwrap_or(path);
            offenders.push(format!("{}:{line}", rel.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "`matches!(..);` drops its result, so these checks never fail. \
         Wrap each in `assert!(matches!(..), \"{{x:?}}\")`:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_scanner_flags_only_discarded_statements() {
    let flagged = |s: &str| bare_matches(s).len();
    assert_eq!(flagged("fn t() { let e = f(); matches!(e, E::A); }"), 1);
    assert_eq!(
        flagged("fn t() {\n    matches!(\n        e,\n        E::A(\")\")\n    );\n}"),
        1
    );
    assert_eq!(flagged("fn t() { assert!(matches!(e, E::A)); }"), 0);
    assert_eq!(flagged("fn is_a(e: &E) -> bool { matches!(e, E::A) }"), 0);
    assert_eq!(flagged("fn t() { x(); matches!(e, E::A) }"), 0);
    assert_eq!(flagged("let ok = matches!(e, E::A);"), 0);
    assert_eq!(flagged("f(|e| matches!(e, E::A));"), 0);
    assert_eq!(flagged("// x;\n// matches!(e, E::A);"), 0);
    assert_eq!(
        flagged("f(|c| {\n    matches!(c, '(' | '\\'' | ')')\n});"),
        0
    );
    assert_eq!(bare_matches("fn t() {\n    matches!(c, ')');\n}"), [2]);
}
