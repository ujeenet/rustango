//! Docs and comments spell env overrides `RUSTANGO__SECTION__KEY`.
//! A single `_` after the prefix is never read, so a doc using it
//! leaves the setting unset (#2257).

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.starts_with("target") || name == "node_modules" {
            continue;
        }
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| {
            ["rs", "md", "toml", "html", "sh", "yml", "env"].contains(&&*x.to_string_lossy())
        }) {
            out.push(p);
        }
    }
}

/// Every `RUSTANGO_X…__…` token that is not `RUSTANGO__…`.
fn bad_spellings(src: &str) -> Vec<&str> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for (i, _) in src.match_indices("RUSTANGO_") {
        if src[..i].chars().next_back().is_some_and(ident) {
            continue;
        }
        let end = src[i..]
            .find(|c: char| !ident(c))
            .map_or(src.len(), |n| i + n);
        let rest = &src[i + "RUSTANGO_".len()..end];
        if !rest.starts_with('_') && rest.contains("__") {
            out.push(&src[i..end]);
        }
    }
    out
}

#[test]
fn env_overrides_use_the_double_underscore() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut all = Vec::new();
    files(&root, &mut all);
    assert!(all.len() > 500, "found only {} files", all.len());

    let me = Path::new(file!()).file_name().unwrap();
    let mut bad = Vec::new();
    for f in &all {
        // The changelog records history, and this file names the typo.
        if f.ends_with("CHANGELOG.md") || f.file_name() == Some(me) {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for tok in bad_spellings(&text) {
            bad.push(format!("  {}: {tok}", f.display()));
        }
    }
    assert!(
        bad.is_empty(),
        "env override spelled with one `_` after RUSTANGO; use `RUSTANGO__SECTION__KEY`:\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_detector_sees_the_typo_only() {
    assert_eq!(
        bad_spellings("set `RUSTANGO_MAIL__SMTP_PASSWORD` here"),
        ["RUSTANGO_MAIL__SMTP_PASSWORD"]
    );
    for ok in [
        "RUSTANGO__MAIL__SMTP_PASSWORD",
        "RUSTANGO_SESSION_SECRET",
        "'nonce-__RUSTANGO_NONCE__'",
        "MY_RUSTANGO_X__Y",
    ] {
        assert!(bad_spellings(ok).is_empty(), "{ok}");
    }
}
