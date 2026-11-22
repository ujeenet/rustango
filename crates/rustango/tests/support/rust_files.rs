//! The file walker the source-scanning guards share.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` under a directory, recursively.
pub fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` under an example is build output, not source.
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
