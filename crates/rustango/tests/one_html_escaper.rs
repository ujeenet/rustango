//! There is one HTML escaper (`text::html_escape`) and one XML escaper
//! (`text::xml_escape_into`); everything else imports them (#1663).
//!
//! Twelve private copies had drifted, two of them missing `'`. A text
//! check, like `one_redirect_sanitizer`: the property is "nobody wrote
//! the escaper again", which is a property of the source.

use std::path::Path;

#[path = "support/rust_files.rs"]
mod rust_files;
use rust_files::rust_files;

/// Lines of non-test code that spell out the `<` entity. Any escaper
/// has to, whatever its shape (match arm, `.replace` chain, table);
/// tests are skipped because they assert on escaped output.
fn entity_lines(src: &str) -> Vec<(usize, &str)> {
    src.lines()
        .enumerate()
        .take_while(|(_, l)| !l.trim_start().starts_with("#[cfg(test)]"))
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .filter(|(_, l)| l.contains("\"&lt;") || l.contains("&#60;"))
        .collect()
}

#[test]
fn only_text_rs_escapes_html() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("examples"), &mut files);
    // Integration tests assert on escaped text.
    files.retain(|p| {
        !p.strip_prefix(root)
            .is_ok_and(|r| r.components().any(|c| c.as_os_str() == "tests"))
    });
    assert!(files.len() > 100, "found only {} sources", files.len());

    let mut copies = Vec::new();
    for f in &files {
        if f.ends_with("src/text.rs") && f.starts_with(root.join("src")) {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for (i, line) in entity_lines(&text) {
            copies.push(format!("  {}:{}: {}", f.display(), i + 1, line.trim()));
        }
    }
    assert!(
        copies.is_empty(),
        "hand-written HTML/XML escaper(s); use `rustango::text::html_escape`:\n{}",
        copies.join("\n")
    );
}

#[test]
fn the_detector_sees_every_shape() {
    for shape in [
        r#"'<' => out.push_str("&lt;"),"#,
        r#".replace("<", "&lt;")"#,
        r#"    "&lt;","#,
        r#"("<", "&#60;")"#,
    ] {
        assert_eq!(entity_lines(shape).len(), 1, "missed: {shape}");
    }
    let tests_only = "fn f() {}\n#[cfg(test)]\nmod t { const X: &str = \"&lt;\"; }";
    assert!(entity_lines(tests_only).is_empty());
    assert!(entity_lines("// \"&lt;\" in a comment").is_empty());
}
