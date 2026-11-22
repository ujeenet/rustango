//! Every public `*Error` enum is `#[non_exhaustive]`, so adding a variant
//! later is not a breaking change.

use std::path::Path;

#[path = "support/rust_files.rs"]
mod rust_files;
use rust_files::rust_files;

/// `file:EnumName` for every `pub enum *Error` whose attributes lack
/// `#[non_exhaustive]`.
fn exhaustive_public_errors() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix("pub enum ") else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.ends_with("Error") {
                continue;
            }
            // Walk up over the doc comments and attributes above the enum.
            let marked = lines[..i]
                .iter()
                .rev()
                .map(|l| l.trim())
                .take_while(|l| l.starts_with("#[") || l.starts_with("//") || l.ends_with(")]"))
                .any(|l| l.contains("non_exhaustive"));
            if !marked {
                let rel = file.strip_prefix(&src).expect("under src");
                let rel = rel.to_string_lossy().replace('\\', "/");
                found.push(format!("{rel}:{name}"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn public_errors_are_non_exhaustive() {
    let found = exhaustive_public_errors();
    assert!(
        found.is_empty(),
        "add #[non_exhaustive] to these public error enums: {found:?}"
    );
}
