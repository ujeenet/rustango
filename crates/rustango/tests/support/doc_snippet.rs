//! The code between `// doc-snippet:start` and `// doc-snippet:end` in a
//! test file must appear verbatim in each locale of a page, so the
//! published example is the compiled one.

/// Assert `page` (e.g. `"security.md"`) and its de/es/fr copies carry the
/// marked snippet of `src` (the calling test's `include_str!` of itself).
pub fn assert_published(src: &str, page: &str) {
    const START: &str = "// doc-snippet:start\n";
    let start = src.find(START).expect("doc-snippet:start marker") + START.len();
    let end = src
        .find("// doc-snippet:end")
        .expect("doc-snippet:end marker");
    let snippet = src[start..end].trim_end();
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for dir in ["", "de", "es", "fr"] {
        let path = docs.join(dir).join(page);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(snippet),
            "{} does not carry the snippet compiled by this test",
            path.display()
        );
    }
}
