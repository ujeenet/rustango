//! Nothing outside the derive macro imports `sql::__macro_internals`
//! (#1431).
//!
//! The module carries "**Not part of the public API; do not import**"
//! and had fourteen call sites across twelve files: nine in-tree
//! tests, four in the `cookbook_blog` example (two in its request
//! handlers, two in its chapter-3 test) and one in **rustango's own
//! library**, `tenancy::permissions`. A reader copying that pattern
//! copied an import the crate forbids, against a function `cargo doc`
//! will not show them because the module is `#[doc(hidden)]`.
//!
//! (The breakdown used to read "ten in-tree tests, the cookbook's
//! chapter-3 test, and two in the flagship example" — which sums to
//! thirteen, counts `cookbook_blog` twice under two names, and omits
//! the library's own call site, the one that most undermines the
//! prohibition. This guard still cannot see that site: it walks
//! `tests/` and `examples/` only — #1519, #1516.)
//!
//! They were not misusing it. There was no public way to run an
//! aggregate or a prefetch against a specific executor rather than a
//! pool, and four of the twelve re-exports — `fetch_aggregate_on`,
//! `select_rows_on`, `annotate_count_children{,_on}` — were never
//! emitted by the macro at all. They had been filed as codegen support
//! and were never that.
//!
//! Those are public now, so the prohibition is one a caller can actually
//! keep. This guard is what makes it stay true: a prohibition the
//! framework's own example violates is not a prohibition, and prose
//! alone did not stop it the first time.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` under a directory, recursively.
fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
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

#[test]
fn nothing_outside_the_macro_imports_macro_internals() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&crate_root.join("tests"), &mut files);
    rust_files(&crate_root.join("examples"), &mut files);

    let mut offenders = Vec::new();
    for path in files {
        // This file names the module to forbid it; so does the test that
        // documents why the `postgres` gate leaked into the derive.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(
            name,
            "macro_internals_stays_internal.rs" | "soft_delete_without_postgres.rs"
        ) {
            continue;
        }
        let Ok(src) = fs::read_to_string(&path) else {
            continue;
        };
        if src.contains("__macro_internals") {
            offenders.push(
                path.strip_prefix(crate_root)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
            );
        }
    }

    offenders.sort();
    assert!(
        offenders.is_empty(),
        "{} file(s) import `sql::__macro_internals`, which is `#[doc(hidden)]` and \
         documented as \"do not import\" (#1431). The executor-taking operations are \
         public — use `rustango::sql::{{fetch_aggregate_on, fetch_with_prefetch, \
         select_rows_on, insert_on, update_on, bulk_insert_on, \
         annotate_count_children, annotate_count_children_on}}`. `delete_on` and \
         `insert_returning_on` are deliberately still hidden — nothing outside \
         codegen had asked for them. If you need one of those, that is a missing \
         public API and belongs on #1431, not an import from here.\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
