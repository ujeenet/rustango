//! #1234 — every `tracing` event must live under the `rustango::` target
//! root, so that the conventional `RUST_LOG=rustango=warn` actually
//! reaches it.
//!
//! `target:` takes a **literal string**, not a path: `target: "crate::x"`
//! compiles happily and then sits in a namespace no realistic filter
//! matches. 48 call sites had drifted that way, several of them the only
//! diagnostic for a deliberately-swallowed failure. The mistake is
//! invisible in review — the string looks like a path — so it is worth a
//! machine check rather than vigilance.

use std::path::Path;

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_tracing_event_uses_a_literal_crate_target() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "found no sources under {}",
        src.display()
    );

    let mut offenders = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line.contains(r#"target: "crate::"#) {
                offenders.push(format!("{}:{}", file.display(), i + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "tracing targets must start with `rustango::`, not the literal \
         `crate::` — `RUST_LOG=rustango=…` cannot match these {} site(s):\n  {}",
        offenders.len(),
        offenders.join("\n  "),
    );
}
