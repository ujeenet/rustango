//! #1248 — every link and image in a published documentation page must
//! resolve, in every locale.
//!
//! A reader found three 404s on the docs site; an audit turned up 160
//! broken references across `en` / `de` / `es` / `fr`. Two failure modes,
//! both invisible on GitHub-rendered English:
//!
//! * **`../`-relative links.** `docs/jobs.md` linking `../crates/…`
//!   resolves on GitHub but not on the docs site, which publishes only
//!   the files listed in `docs/index.toml` and has no `crates/` tree.
//!   In a locale the same path is worse — from `docs/fr/` it points at
//!   `docs/crates/`, which has never existed.
//! * **Locale images.** Translations copied `img/foo.png` verbatim, but
//!   images live only in `docs/img/`, so every hero image in `de` / `es`
//!   / `fr` was broken.
//!
//! Repo links now use absolute `https://github.com/…` URLs, which work
//! from any renderer. This test keeps it that way.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/rustango.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root")
}

/// Pages the docs site actually publishes, per `docs/index.toml`.
/// Anything absent is intentionally unpublished (internal audits), so a
/// link to it 404s for readers.
fn published_pages(root: &Path) -> HashSet<String> {
    let toml = std::fs::read_to_string(root.join("docs/index.toml")).expect("read docs/index.toml");
    let mut out = HashSet::new();
    let mut rest = toml.as_str();
    while let Some(i) = rest.find('"') {
        rest = &rest[i + 1..];
        let Some(j) = rest.find('"') else { break };
        let val = &rest[..j];
        rest = &rest[j + 1..];
        if val.ends_with(".md") {
            out.insert(val.to_owned());
        }
    }
    assert!(!out.is_empty(), "parsed no pages out of docs/index.toml");
    out
}

/// Markdown link/image targets: the `(...)` of `[..](..)` and `![..](..)`.
fn link_targets(text: &str) -> Vec<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == ']' && i + 1 < bytes.len() && bytes[i + 1] == '(' {
            let mut j = i + 2;
            let mut buf = String::new();
            while j < bytes.len() && bytes[j] != ')' && !bytes[j].is_whitespace() {
                buf.push(bytes[j]);
                j += 1;
            }
            if j < bytes.len() && bytes[j] == ')' && !buf.is_empty() {
                out.push(buf);
            }
            i = j;
        }
        i += 1;
    }
    out
}

#[test]
fn every_published_doc_link_resolves_in_every_locale() {
    let root = repo_root();
    let published = published_pages(&root);
    let mut problems = Vec::new();
    let mut checked = 0usize;

    for locale_dir in ["docs", "docs/de", "docs/es", "docs/fr"] {
        for page in &published {
            let path = root.join(locale_dir).join(page);
            if !path.exists() {
                problems.push(format!(
                    "{locale_dir}/{page}: published in index.toml but the file is missing"
                ));
                continue;
            }
            checked += 1;
            let text = std::fs::read_to_string(&path).expect("read page");
            for target in link_targets(&text) {
                if target.starts_with("http")
                    || target.starts_with('#')
                    || target.starts_with("mailto")
                {
                    continue;
                }
                // A `../` escape leaves the published tree: it cannot
                // resolve on the docs site, and in a locale it does not
                // resolve on GitHub either. Link the repo absolutely.
                if target.starts_with("../") && !target.starts_with("../img/") {
                    problems.push(format!(
                        "{locale_dir}/{page}: `{target}` escapes the docs tree — \
                         use an absolute https://github.com/ujeenet/rustango/... URL"
                    ));
                    continue;
                }
                let base = target.split('#').next().unwrap_or(&target);
                let resolved = path.parent().expect("page has a parent").join(base);
                if !resolved.exists() {
                    problems.push(format!("{locale_dir}/{page}: `{target}` does not resolve"));
                    continue;
                }
                // A link to a markdown file that the site does not
                // publish 404s for readers even though it exists here.
                if base.ends_with(".md") && !published.contains(base.trim_start_matches("./")) {
                    problems.push(format!(
                        "{locale_dir}/{page}: `{target}` targets a page that index.toml \
                         does not publish"
                    ));
                }
            }
        }
    }

    assert!(checked > 0, "checked no pages at all");
    assert!(
        problems.is_empty(),
        "{} broken documentation reference(s) across {checked} pages:\n  {}",
        problems.len(),
        problems.join("\n  "),
    );
}

/// Translations must cover exactly what the site publishes — a missing
/// locale page is a 404 for that language, an extra one is unreachable.
#[test]
fn every_locale_has_every_published_page() {
    let root = repo_root();
    let published = published_pages(&root);
    let mut problems = Vec::new();

    for locale in ["de", "es", "fr"] {
        let dir = root.join("docs").join(locale);
        let have: HashSet<String> = std::fs::read_dir(&dir)
            .expect("read locale dir")
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".md"))
            .collect();
        for missing in published.difference(&have) {
            problems.push(format!("docs/{locale}/{missing} is missing"));
        }
        for extra in have.difference(&published) {
            problems.push(format!(
                "docs/{locale}/{extra} is not published by index.toml"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "locale coverage gaps:\n  {}",
        problems.join("\n  "),
    );
}
