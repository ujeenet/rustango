//! Guard: a green run with no `DATABASE_URL` must not look like a real one.
//!
//! Most chapter suites open with `let Some(pool) = pool().await else { return };`
//! and `pool()` is `std::env::var("DATABASE_URL").ok()?`. With no database the
//! test returns immediately and libtest counts it **passed** — so the whole
//! crate reports the same pass count either way and a reader cannot tell the
//! two runs apart from the result line.
//!
//! This guard is the difference. With `DATABASE_URL` set it passes silently;
//! without one it fails and names the suites that did not actually run. CI
//! always sets `DATABASE_URL` for this crate (`doc_examples` in ci.yml), so
//! this is red only where the evidence really is missing.
//!
//! To work on the no-database chapters alone, skip this file:
//! `cargo test --test cookbook_chapter07_forms` (or any other single target).

use std::fs;
use std::path::PathBuf;

/// The idiom that turns an absent database into a silent pass.
const SILENT_SKIP: &str = "else { return }";

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Every chapter suite whose tests evaporate without a database.
fn suites_that_need_a_database() -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(tests_dir())
        .expect("tests/ is readable")
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "rs" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_owned();
            // Don't count this guard itself.
            if name == "a_green_run_without_a_database_proves_little" {
                return None;
            }
            let body = fs::read_to_string(&path).ok()?;
            // `DATABASE_URL` alone is not enough — a suite may name it in a
            // doc comment. The silent-skip idiom is what makes it evaporate.
            (body.contains("DATABASE_URL") && body.contains(SILENT_SKIP)).then_some(name)
        })
        .collect();
    found.sort();
    found
}

#[test]
fn the_database_backed_chapters_actually_ran() {
    let suites = suites_that_need_a_database();

    // The guard must not go inert if the idiom is ever refactored away.
    assert!(
        !suites.is_empty(),
        "found no suite using `{SILENT_SKIP}` with DATABASE_URL. Either the \
         chapters stopped skipping silently — in which case delete this guard \
         — or the idiom changed and SILENT_SKIP needs updating. Do not leave \
         it matching nothing: it would pass forever.",
    );

    if std::env::var("DATABASE_URL").is_ok() {
        return;
    }

    let list = suites
        .iter()
        .map(|s| format!("  - {s}"))
        .collect::<Vec<_>>()
        .join("\n");

    panic!(
        "DATABASE_URL is unset, so {} of this crate's chapter suites did not \
         run — their tests returned immediately and libtest counted them \
         passed:\n\n{list}\n\n\
         The rest of the run is green, and that green says nothing about the \
         recipes in those chapters. Start Postgres and set DATABASE_URL:\n\n  \
         docker compose up -d postgres\n  \
         export DATABASE_URL=postgres://rustango:rustango@localhost:5432/rustango_test\n  \
         cargo test -- --test-threads=1\n\n\
         To work on a chapter that needs no database, run that target alone:\n  \
         cargo test --test cookbook_chapter07_forms",
        suites.len(),
    );
}
