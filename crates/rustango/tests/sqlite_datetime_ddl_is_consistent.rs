//! No `SQLite` DDL in this crate may default a datetime to
//! `CURRENT_TIMESTAMP` (#1464).
//!
//! `SQLite` has no datetime type, so such a column is TEXT and compares
//! **lexicographically**. `CURRENT_TIMESTAMP` writes
//! `YYYY-MM-DD HH:MM:SS`, sqlx binds a `DateTime<Utc>` as RFC3339, and
//! the two diverge at position 10 — `' '` (0x20) against `'T'` (0x54).
//! Every comparison against such a column was therefore true for every
//! row, whatever was bound.
//!
//! The model-derived tables are fixed centrally, in the dialect's
//! `translate_default_expr`. The tables whose `CREATE TABLE` text this
//! crate writes by hand are not covered by that, and there is nothing
//! in the type system to stop the next hand-written table
//! reintroducing the bug — it is a plain string. So this reads the
//! source.
//!
//! ## Why a source scan rather than a behavioural test
//!
//! A behavioural test can only check the tables that exist today. The
//! failure this guards against is a *new* hand-written table added
//! next year by someone who has not read #1464 — and the natural
//! spelling for them to reach for is the one that is wrong. That
//! defect is visible in the source and invisible to any test of
//! current behaviour.
//!
//! `MySQL`'s `CURRENT_TIMESTAMP(6)` and Postgres' `NOW()` are correct on
//! those backends and are not matched here: `MySQL` and Postgres have
//! real datetime types and never had this defect.

use std::path::{Path, PathBuf};

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, recursively.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_sqlite_ddl_defaults_a_datetime_to_current_timestamp() {
    let mut files = Vec::new();
    sources(&src_root(), &mut files);
    assert!(
        files.len() > 50,
        "found only {} source files — the scan is not reading the tree \
         it thinks it is",
        files.len()
    );

    let mut hits = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let Some(pos) = line.find("DEFAULT CURRENT_TIMESTAMP") else {
                continue;
            };
            // `CURRENT_TIMESTAMP(6)` is MySQL's fractional-precision
            // form and is correct there. Only the bare spelling is a
            // candidate.
            let rest = &line[pos + "DEFAULT CURRENT_TIMESTAMP".len()..];
            if rest.starts_with('(') {
                continue;
            }
            // The defect needs a **TEXT** column: it is lexicographic
            // comparison of a formatted string that goes wrong, and
            // only SQLite stores a datetime that way. MySQL's
            // `TIMESTAMP`/`DATETIME` and Postgres' `TIMESTAMPTZ` are
            // real datetime types and compare as instants, so a bare
            // `DEFAULT CURRENT_TIMESTAMP` on those is correct.
            //
            // Keying on the type rather than on which `const` block the
            // line sits in: the first version of this guard matched the
            // whole line and flagged `rustango_translations`' MySQL DDL,
            // which is right as written. The type is what actually
            // decides whether the bug is possible.
            if !line.contains("TEXT") {
                continue;
            }
            // A line that is prose about the defect rather than DDL
            // emitting it. Anchored on the comment markers actually
            // used in this crate — `//`, `///`, `//!` and SQL `--` —
            // tested against the trimmed line so an indented comment
            // is still recognised.
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("--") || t.starts_with("* ") {
                continue;
            }
            hits.push(format!(
                "{}:{}: {}",
                path.strip_prefix(src_root().parent().unwrap_or(Path::new("")))
                    .unwrap_or(path)
                    .display(),
                i + 1,
                line.trim()
            ));
        }
    }

    assert!(
        hits.is_empty(),
        "SQLite DDL must not default a datetime column to \
         `CURRENT_TIMESTAMP` — it writes `YYYY-MM-DD HH:MM:SS`, which \
         does not compare or sort against the RFC3339 sqlx binds for a \
         `DateTime<Utc>` (#1464).\n\n{}\n\nUse the same shape the \
         model-derived tables emit:\n    \
         DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000+00:00','now'))\n\
         which is `sql::sqlite::SQLITE_DATETIME_FORMAT`. MySQL's \
         `CURRENT_TIMESTAMP(6)` and Postgres' `NOW()` are unaffected \
         and are not matched by this guard.",
        hits.join("\n"),
    );
}

/// The canonical shape, spelled once.
///
/// Duplicated from `sql::sqlite::SQLITE_DATETIME_FORMAT`, which is
/// `pub(crate)` and so unreachable from an integration test. That is
/// the drift this guard accepts in order to prevent a worse one, and
/// `the_canonical_format_is_still_what_the_crate_uses` pins the copy
/// against the source.
const CANONICAL: &str = "%Y-%m-%dT%H:%M:%f000+00:00";

/// A `strftime` DDL default must use the canonical format, not merely
/// avoid `CURRENT_TIMESTAMP`.
///
/// Banning one wrong spelling leaves every other wrong spelling
/// allowed, and one was already in the tree: `rustango_jobs` defaulted
/// to `%Y-%m-%dT%H:%M:%fZ` — `Z` suffix, three digits — which for the
/// same instant is a different string from the six-digit `+00:00`
/// shape everything else writes. Two canonical formats is the same
/// defect as one canonical and one legacy; the column still holds two
/// spellings and comparisons across them are still wrong.
#[test]
fn every_sqlite_strftime_default_uses_the_canonical_format() {
    let mut files = Vec::new();
    sources(&src_root(), &mut files);

    let mut hits = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            // Only DDL defaults. `strftime` also appears in query
            // emitters (`date_hierarchy`, `trunc`), where the format is
            // the caller's business and has nothing to do with how a
            // column is stored.
            if !line.contains("DEFAULT (strftime(") {
                continue;
            }
            if line.contains(CANONICAL) {
                continue;
            }
            hits.push(format!(
                "{}:{}: {}",
                path.strip_prefix(src_root().parent().unwrap_or(Path::new("")))
                    .unwrap_or(path)
                    .display(),
                i + 1,
                line.trim()
            ));
        }
    }

    assert!(
        hits.is_empty(),
        "a SQLite `DEFAULT (strftime(…))` must use the canonical format \
         `{CANONICAL}` (#1464).\n\n{}\n\nA second strftime shape is not \
         an improvement on `CURRENT_TIMESTAMP`: the column still ends up \
         holding two spellings of the same instant, and text comparison \
         still gets them wrong. `rustango_jobs` was in exactly this \
         state with a `%fZ` default.",
        hits.join("\n"),
    );
}

/// The copy above must match what the crate actually emits.
///
/// Without this the guard could enforce a format the code stopped
/// using — passing while every column drifted somewhere else, which is
/// the failure mode it exists to prevent, one level up.
#[test]
fn the_canonical_format_is_still_what_the_crate_uses() {
    let sqlite_rs = std::fs::read_to_string(src_root().join("sql/sqlite.rs")).expect("read");
    assert!(
        sqlite_rs.contains(&format!(
            "pub(crate) const SQLITE_DATETIME_FORMAT: &str = \"{CANONICAL}\";"
        )),
        "this file's CANONICAL copy no longer matches \
         `SQLITE_DATETIME_FORMAT` in sql/sqlite.rs. Update both, or the \
         guard above is enforcing a format nothing writes."
    );
}

/// The guard above is a negative assertion, so it passes on a tree
/// where it matches nothing for the wrong reason — a broken scan, a
/// changed spelling. This proves the matcher still recognises the
/// defect it is looking for.
#[test]
fn the_scan_still_recognises_the_defect() {
    let sqlite_shape = r#"    "created_at" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,"#;
    let mysql_fractional = "    `created_at` DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),";
    // The shape that made the first version of this guard fail on
    // correct code: MySQL's `TIMESTAMP` with a bare default.
    let mysql_bare = "    `created_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,";
    let comment = "    // was DEFAULT CURRENT_TIMESTAMP before #1464";

    let is_hit = |line: &str| {
        let Some(pos) = line.find("DEFAULT CURRENT_TIMESTAMP") else {
            return false;
        };
        let rest = &line[pos + "DEFAULT CURRENT_TIMESTAMP".len()..];
        if rest.starts_with('(') {
            return false;
        }
        if !line.contains("TEXT") {
            return false;
        }
        let t = line.trim_start();
        !(t.starts_with("//") || t.starts_with("--") || t.starts_with("* "))
    };

    assert!(
        is_hit(sqlite_shape),
        "must flag a TEXT column with a bare CURRENT_TIMESTAMP default"
    );
    assert!(
        !is_hit(mysql_fractional),
        "must not flag MySQL's CURRENT_TIMESTAMP(6)"
    );
    assert!(
        !is_hit(mysql_bare),
        "must not flag a bare default on MySQL's real TIMESTAMP type — \
         that column compares as an instant, not as text"
    );
    assert!(!is_hit(comment), "must not flag prose about the defect");
}
