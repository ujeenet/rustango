//! Sample output in the docs must show the version that is actually
//! shipping.
//!
//! `docs/manage.md` told readers that `manage version` prints
//! `rustango 0.44.0`, and `docs/mcp.md` showed an MCP handshake
//! answering `"version": "0.44.0"` — in all four locales, thirteen
//! releases after 0.44.0. Nothing was wrong with the prose; the
//! transcripts had simply been frozen when they were written.
//!
//! That is what a release checklist is bad at. It decays silently, and
//! nobody notices, because the page still reads correctly. So this is a
//! test instead.
//!
//! ## What counts as a version mention
//!
//! Only versions **rustango is claiming as its own**, which in practice
//! means the token `rustango` sits next to them:
//!
//! ```text
//! rustango 0.44.0                                    ← same line
//! "name": "rustango", "version": "0.44.0"            ← same line
//! rustango
//!   version:        0.44.0                           ← `about` transcript
//! ```
//!
//! Everything else in the docs is left alone, and deliberately. The
//! pages are full of three-component numbers that have nothing to do
//! with this crate's version — `OpenApiSpec::new("Blog API", "1.0.0")`
//! is the *reader's* API version, `"openapi": "3.1.0"` is the spec,
//! `MySQL 8.0.1+` is a database. A test that flagged those would be
//! turned off within a week.
//!
//! Prose about old releases is also untouched, because it is correct:
//! "before v0.24.2", "Introduced in 0.51.1", "0.51.0 and 0.51.1 were
//! yanked" are all statements about history that do not change when a
//! new version ships. None of them put `rustango` next to the number,
//! which is what keeps them out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The version this build ships as — the string `manage version` prints.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root")
}

/// Pages the docs site publishes, per `docs/index.toml` — the same set
/// `docs_links` checks.
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

/// Does `line` use `rustango` as a word, rather than as part of a longer
/// identifier?
///
/// `rustango 0.44.0` yes; `rustango::openapi` and `cargo-rustango` no —
/// a `use` line naming a module is not the framework announcing its
/// version, and that distinction is what keeps
/// `OpenApiSpec::new("Blog API", "1.0.0")` out of the results.
fn names_rustango(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(hit) = lower[from..].find("rustango") {
        let start = from + hit;
        let end = start + "rustango".len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':' || b == b'/'
}

/// Every `MAJOR.MINOR.PATCH` in `line` that is *labelled* as rustango's.
///
/// The label is the word immediately before the number, ignoring the
/// quotes and punctuation between them — `rustango` or `version`, and
/// nothing else. Requiring it is what separates
///
/// ```text
/// rustango 0.44.0                    ← labelled `rustango`
/// "name": "rustango", "version": "0.44.0"   ← labelled `version`
/// ```
///
/// from a line that merely happens to mention the framework:
///
/// ```text
/// …on every version **Rustango** supports … landed in MySQL 8.0.31.
/// ```
///
/// which is labelled `MySQL` and is none of this test's business.
///
/// Three components on purpose: a two-component string is a dependency
/// caret (`axum = "0.8"`) and says nothing about rustango. The
/// neighbour checks drop anything inside a longer dotted run, which is
/// how `0.0.0.0:8080` — the README's bind address — stays out.
fn versions_in(line: &str) -> Vec<String> {
    let b: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && (b[i - 1] == '.' || b[i - 1].is_ascii_digit())) {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        while j < b.len() && (b[j].is_ascii_digit() || b[j] == '.') {
            j += 1;
        }
        // A trailing `.` is sentence punctuation, not part of the number.
        let mut end = j;
        while end > start && b[end - 1] == '.' {
            end -= 1;
        }
        let s: String = b[start..end].iter().collect();
        if s.split('.').count() == 3
            && matches!(
                label_before(&b, start).as_deref(),
                Some("rustango" | "version")
            )
        {
            out.push(s);
        }
        i = j.max(start + 1);
    }
    out
}

/// The word before position `start`, lowercased, skipping the quotes,
/// colons, equals and whitespace that sit between a label and its value.
fn label_before(b: &[char], start: usize) -> Option<String> {
    let mut k = start;
    while k > 0 && !b[k - 1].is_alphanumeric() {
        k -= 1;
    }
    let word_end = k;
    while k > 0 && b[k - 1].is_alphanumeric() {
        k -= 1;
    }
    (k < word_end).then(|| {
        b[k..word_end]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase()
    })
}

/// Versions the page presents as rustango's own, with line numbers.
fn claimed_versions(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    // The `about` transcript prints a bare `rustango` line and then an
    // indented `version:` field, so the subject is on a previous line.
    // Scoped to the current fenced block so a stray `rustango` in prose
    // cannot adopt an unrelated `version:` further down the page.
    let mut in_fence = false;
    let mut fence_names_rustango = false;

    for (idx, line) in text.lines().enumerate() {
        let lineno = idx + 1;
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            fence_names_rustango = false;
            continue;
        }
        if in_fence && line.trim() == "rustango" {
            fence_names_rustango = true;
        }

        let is_version_field = line.trim_start().starts_with("version:");
        let claims = names_rustango(line) || (in_fence && fence_names_rustango && is_version_field);
        if !claims {
            continue;
        }
        for v in versions_in(line) {
            out.push((lineno, v));
        }
    }
    out
}

#[test]
fn published_docs_show_the_version_that_is_shipping() {
    let root = repo_root();
    let published = published_pages(&root);
    let mut problems = Vec::new();
    let mut checked = 0usize;

    let mut pages: Vec<String> = Vec::new();
    for locale_dir in ["docs", "docs/de", "docs/es", "docs/fr"] {
        for page in &published {
            pages.push(format!("{locale_dir}/{page}"));
        }
    }
    // Reader-facing prose outside `docs/`. The renamed-smoke README
    // quotes its own `Cargo.toml`, and had drifted three releases from
    // the file it was describing.
    pages.push("README.md".to_owned());
    pages.push("crates/rustango-renamed-smoke/README.md".to_owned());
    pages.sort();

    for rel in &pages {
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        checked += 1;
        for (lineno, found) in claimed_versions(&text) {
            if found != CURRENT {
                problems.push(format!("{rel}:{lineno}: shows `{found}`"));
            }
        }
    }

    assert!(checked > 0, "checked no pages at all");
    assert!(
        problems.is_empty(),
        "{} doc(s) present a rustango version that is not the one shipping ({CURRENT}):\n  {}\n\n\
         These are transcripts of rustango's own output, so a frozen one reads as correct \
         and is not. Update them to {CURRENT}. Prose *about* an older release is fine and \
         is not checked — it is only flagged when `rustango` sits next to the number.",
        problems.len(),
        problems.join("\n  "),
    );
}

/// `docs/index.toml`'s `version` is the label the site publishes these
/// pages **under**: they are served at `/<version>/<section>/<slug>`.
///
/// It is checked separately from the prose above because it is a
/// different kind of claim and a different shape — `major.minor`, no
/// patch, and the whole file is one line of TOML rather than a
/// transcript. It had drifted to `0.53` while 0.57.0 was shipping, which
/// does not read as wrong anywhere: the pages are correct, the nav is
/// correct, and the current documentation is quietly served under a
/// four-release-old URL.
#[test]
fn the_docs_manifest_publishes_under_the_shipping_version() {
    let root = repo_root();
    let toml = std::fs::read_to_string(root.join("docs/index.toml")).expect("read docs/index.toml");

    let declared = toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("version"))
        .and_then(|rest| rest.trim().strip_prefix('='))
        .map(|rest| rest.trim().trim_matches('"').to_owned())
        .expect("docs/index.toml declares a `version`");

    // The manifest labels a doc *series*, not a point release, so it
    // carries major.minor: 0.57.1 still publishes under /0.57.
    let expected: String = CURRENT.rsplit_once('.').map_or_else(
        || CURRENT.to_owned(),
        |(major_minor, _patch)| major_minor.to_owned(),
    );

    assert_eq!(
        declared, expected,
        "docs/index.toml publishes under `/{declared}/` but this build ships {CURRENT}. \
         Set it to \"{expected}\" — otherwise the current documentation is served at a \
         stale URL and no page looks wrong."
    );
}

#[cfg(test)]
mod tests {
    use super::{claimed_versions, names_rustango, versions_in};

    #[test]
    fn a_dependency_caret_is_not_a_version() {
        assert!(versions_in(r#"axum = { version = "0.8" }"#).is_empty());
    }

    /// Four components, not three. This is the README's bind address,
    /// and matching `0.0.0` inside it is the false positive that makes
    /// a naive version regex useless.
    #[test]
    fn a_bind_address_is_not_a_version() {
        assert!(versions_in(r#".serve("0.0.0.0:8080").await"#).is_empty());
    }

    #[test]
    fn a_trailing_full_stop_is_not_part_of_the_version() {
        assert_eq!(versions_in("rustango 1.2.3."), vec!["1.2.3".to_owned()]);
    }

    /// The label is what makes a number rustango's. `MySQL 8.0.31` on a
    /// line that also says "Rustango" is about MySQL, and flagging it
    /// would be the kind of noise that gets a test deleted.
    #[test]
    fn an_unlabelled_version_is_ignored_even_beside_the_word_rustango() {
        let line = "…on every version **Rustango** supports. `INTERSECT` landed in MySQL 8.0.31.";
        assert!(versions_in(line).is_empty());
    }

    #[test]
    fn both_labels_are_recognized() {
        assert_eq!(versions_in("rustango 0.57.0"), vec!["0.57.0".to_owned()]);
        assert_eq!(
            versions_in(r#""version": "0.57.0""#),
            vec!["0.57.0".to_owned()]
        );
    }

    #[test]
    fn rustango_must_be_a_word_not_a_prefix() {
        assert!(names_rustango("rustango 0.44.0"));
        assert!(names_rustango(r#""name": "rustango", "version": "0.44.0""#));
        // A module path or the scaffolder binary is not the framework
        // announcing a version.
        assert!(!names_rustango("use rustango::openapi::OpenApiSpec;"));
        assert!(!names_rustango("cargo install cargo-rustango"));
    }

    #[test]
    fn a_version_transcript_is_claimed() {
        assert_eq!(
            claimed_versions("```bash\n$ cargo run -- version\nrustango 0.44.0\n```"),
            vec![(3, "0.44.0".to_owned())]
        );
    }

    /// The `about` transcript names rustango on its own line and puts
    /// the number on the next one.
    #[test]
    fn an_about_transcript_is_claimed_across_lines() {
        let text = "```bash\n$ cargo run -- about\nrustango\n  version:        0.44.0\n```";
        assert_eq!(claimed_versions(text), vec![(4, "0.44.0".to_owned())]);
    }

    /// The reader's own API version, in a fence that happens to import
    /// rustango. Flagging this is what would get the test deleted.
    #[test]
    fn a_readers_own_api_version_is_not_claimed() {
        let text = "```rust\nuse rustango::openapi::OpenApiSpec;\n\
                    let spec = OpenApiSpec::new(\"Blog API\", \"1.0.0\");\n```";
        assert!(claimed_versions(text).is_empty());
    }

    /// Prose about history is correct and must stay untouched.
    #[test]
    fn prose_about_an_older_release_is_not_claimed() {
        assert!(claimed_versions("Introduced in 0.51.1; see the CHANGELOG.").is_empty());
        assert!(claimed_versions("> **0.51.0 and 0.51.1 were yanked** — the").is_empty());
        assert!(claimed_versions("Why the split matters: before v0.24.2, a plain").is_empty());
        assert!(claimed_versions("## Tenant-pool tuning (v0.27.7+)").is_empty());
    }

    /// A `version:` field outside a rustango transcript stays out, even
    /// in a fenced block.
    #[test]
    fn an_unrelated_version_field_is_not_claimed() {
        let text = "```yaml\nservices:\n  version:        3.9.0\n```";
        assert!(claimed_versions(text).is_empty());
    }
}
