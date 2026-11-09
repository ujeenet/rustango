//! The guide's Step 14 code, in every language, is the code this example
//! compiles and `auth_flow.rs` runs. No database needed.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p.as_ref()).unwrap_or_else(|e| panic!("{}: {e}", p.as_ref().display()))
}

/// The ```rust fences between `from` and the next `## ` heading.
fn rust_blocks(page: &str, from: &str) -> Vec<String> {
    let start = page.find(from).unwrap_or_else(|| panic!("no `{from}`"));
    let rest = &page[start + from.len()..];
    let section = &rest[..rest.find("\n## ").unwrap_or(rest.len())];
    section
        .split("```rust\n")
        .skip(1)
        .map(|b| b[..b.find("\n```").expect("closed fence")].to_owned())
        .collect()
}

/// `block`'s non-empty lines appear in `file`, in order.
fn contained(block: &str, file: &str) -> bool {
    let mut lines = file.lines().map(str::trim);
    block
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .all(|want| lines.any(|have| have == want))
}

fn sources() -> Vec<(String, String)> {
    ["src/blog/auth.rs", "src/blog/models.rs", "src/blog/urls.rs", "src/urls.rs", "src/main.rs", "src/post_view_set.rs"]
        .iter()
        .map(|f| (f.to_string(), read(root().join(f))))
        .collect()
}

fn assert_in_sources(blocks: &[String], what: &str) {
    let sources = sources();
    for block in blocks {
        assert!(
            sources.iter().any(|(_, text)| contained(block, text)),
            "{what}: this block matches no example source file:\n{block}"
        );
    }
}

const PAGES: [(&str, &str); 4] = [
    ("", "## Step 14:"),
    ("de/", "## Schritt 14:"),
    ("es/", "## Paso 14:"),
    ("fr/", "## Étape 14 :"),
];

#[test]
fn step_14_code_is_the_example_code_in_every_language() {
    let docs = root().join("../../../../docs");
    let en = rust_blocks(&read(docs.join("getting-started.md")), PAGES[0].1);
    assert!(en.len() >= 8, "found only {} Step 14 blocks", en.len());
    assert_in_sources(&en, "docs/getting-started.md");
    for (dir, heading) in &PAGES[1..] {
        let page = read(docs.join(format!("{dir}getting-started.md")));
        assert_eq!(rust_blocks(&page, heading), en, "docs/{dir}getting-started.md Step 14 code differs from English");
    }
}

#[test]
fn the_login_handler_on_the_passwords_page_is_the_example_one() {
    let docs = root().join("../../../../docs");
    for (dir, _) in PAGES {
        let page = read(docs.join(format!("{dir}auth-passwords.md")));
        let block: Vec<String> = page
            .split("```rust\n")
            .skip(1)
            .map(|b| b[..b.find("```").expect("closed fence")].trim_end().to_owned())
            .filter(|b| b.contains("pub async fn login("))
            .collect();
        assert_eq!(block.len(), 1, "docs/{dir}auth-passwords.md");
        // Its imports are a subset of `auth.rs`'s; the handler itself is verbatim.
        let body = block[0][block[0].find("/// `POST /api/login`").unwrap()..].to_owned();
        assert_in_sources(&[body], &format!("docs/{dir}auth-passwords.md"));
    }
}
