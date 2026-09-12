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

/// The id a heading gets, GitHub-style — which is what the docs site and
/// GitHub both use.
///
/// Lowercase, drop punctuation but keep Unicode letters, spaces to
/// hyphens. A dropped separator leaves its spaces behind, so `Auth &
/// tenancy` becomes `auth--tenancy` with two hyphens.
///
/// Verified against the English pages, which resolve 100% under this rule
/// (#1354). That is what makes it safe to assert on the translations.
fn heading_slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .replace('`', "")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .replace(' ', "-")
}

/// Every heading id a page offers.
fn heading_ids(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|l| {
            let t = l.trim_end();
            let rest = t.trim_start_matches('#');
            // A heading is `#`+ then a space; `#!/bin/sh` inside a fence is not.
            (t.starts_with('#') && rest.starts_with(' ')).then(|| heading_slug(rest))
        })
        .collect()
}

/// `#fragment` links must resolve too (#1354).
///
/// The translations translated their **headings** and kept the **English
/// anchors**, so every table of contents in `de` / `fr` / `es` pointed at
/// ids that exist only in the English file — 886 dead links across 91
/// pages, invisible to the check below because it skips fragments.
#[test]
fn every_anchor_resolves_in_every_locale() {
    let root = repo_root();
    let published = published_pages(&root);
    let mut problems = Vec::new();

    for locale_dir in ["docs", "docs/de", "docs/es", "docs/fr"] {
        for page in &published {
            let path = root.join(locale_dir).join(page);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let own = heading_ids(&text);
            for target in link_targets(&text) {
                if target.starts_with("http") || target.starts_with("mailto") {
                    continue;
                }
                let (file, anchor) = match target.split_once('#') {
                    Some((f, a)) if !a.is_empty() => (f, a),
                    _ => continue,
                };
                // Same page, or a heading in a sibling page.
                let ids = if file.is_empty() {
                    own.clone()
                } else {
                    let sibling = path.parent().expect("parent").join(file);
                    match std::fs::read_to_string(&sibling) {
                        Ok(t) => heading_ids(&t),
                        // A missing file is the other test's business.
                        Err(_) => continue,
                    }
                };
                if !ids.contains(anchor) {
                    problems.push(format!(
                        "{locale_dir}/{page}: `#{anchor}`{} matches no heading",
                        if file.is_empty() {
                            String::new()
                        } else {
                            format!(" in {file}")
                        }
                    ));
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "{} dead anchor(s):\n  {}\n\nThe id is the heading text: lowercased, \
         punctuation dropped, spaces to hyphens. Translating a heading changes \
         its id, so the links pointing at it have to move too.",
        problems.len(),
        problems.join("\n  "),
    );
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
