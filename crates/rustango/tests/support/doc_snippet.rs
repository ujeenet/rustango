//! The code between `// doc-snippet:start` and `// doc-snippet:end` in a
//! test file must appear verbatim in each locale of a page, so the
//! published example is the compiled one.
//!
//! A file may mark several snippets. A marker may be indented, for a
//! snippet inside a module or function: that indent is removed from each
//! line of the snippet before the page is searched.

/// Assert `page` (e.g. `"security.md"`) and its de/es/fr copies carry every
/// marked snippet of `src` (the calling test's `include_str!` of itself).
pub fn assert_published(src: &str, page: &str) {
    let snippets = snippets(src);
    assert!(!snippets.is_empty(), "no doc-snippet:start marker");
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for dir in ["", "de", "es", "fr"] {
        let path = docs.join(dir).join(page);
        let text = std::fs::read_to_string(&path).unwrap();
        for snippet in &snippets {
            let first = snippet.lines().next().unwrap_or("");
            assert!(
                text.contains(snippet.as_str()),
                "{} does not carry the snippet compiled by this test, starting `{first}`",
                path.display()
            );
        }
    }
}

/// Each marked snippet, with its marker's indent removed.
fn snippets(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "// doc-snippet:start" {
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        let mut snippet = String::new();
        for line in lines.by_ref() {
            if line.trim() == "// doc-snippet:end" {
                break;
            }
            snippet.push_str(line.strip_prefix(indent).unwrap_or(line));
            snippet.push('\n');
        }
        out.push(snippet.trim_end().to_owned());
    }
    out
}
