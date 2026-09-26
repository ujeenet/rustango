//! There is one HTML escaper (`text::html_escape`) and one XML escaper
//! (`text::escape_xml_into`); everything else imports them (#1663).
//!
//! Twelve private copies had drifted, three of them missing `'`, so an
//! attribute written with single quotes could be broken out of. A text
//! check, like `one_redirect_sanitizer`: the property is "nobody wrote
//! the escaper again", which is a property of the source.

use std::path::{Path, PathBuf};

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// A line mapping `<` to `&lt;` is an escaper, in any of the shapes the
/// copies used (`'<' => … "&lt;"`, `.replace('<', "&lt;")`).
fn is_escaper_line(line: &str) -> bool {
    line.contains("'<'") && line.contains("\"&lt;\"")
}

#[test]
fn only_text_rs_escapes_html() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "found no sources under {}",
        src.display()
    );

    let mut copies = Vec::new();
    for f in &files {
        if f.ends_with("src/text.rs") {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for (i, line) in text.lines().enumerate() {
            if is_escaper_line(line) {
                copies.push(format!("  {}:{}: {}", f.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        copies.is_empty(),
        "hand-written HTML/XML escaper(s); use `crate::text::html_escape` or \
         `crate::text::escape_xml_into`:\n{}",
        copies.join("\n")
    );
}

#[test]
fn the_detector_sees_both_shapes() {
    assert!(is_escaper_line(r#"'<' => out.push_str("&lt;"),"#));
    assert!(is_escaper_line(r#".replace('<', "&lt;")"#));
    assert!(!is_escaper_line(r#"assert!(html.contains("&lt;"));"#));
}
