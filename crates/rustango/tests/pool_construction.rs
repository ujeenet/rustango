//! Production code builds pools through one constructor.
//!
//! Pool construction had spread to ~22 production sites, and most of
//! them called sqlx directly — `PgPool::connect(&url)`,
//! `SqlitePoolOptions::new()`. Those calls apply none of the options
//! the framework sets, so the settings that *were* wired up reached
//! almost nothing. The main `runserver` pool was one of the bare ones:
//! the single most important pool in a running app, built with sqlx's
//! defaults.
//!
//! Routing them through `Pool::connect*` is only half a fix, because
//! nothing stops the next one being written the old way — and it would
//! not fail any test. Both times this pattern regrew, it regrew
//! silently. So this is the guard.
//!
//! Test fixtures are exempt and deliberately so: they build
//! `max_connections(1)` lazy pools pointed at unreachable hosts, where
//! framework options are noise.

use std::path::{Path, PathBuf};

/// The calls that bypass the framework's own constructors.
const BARE: &[&str] = &[
    "PgPool::connect",
    "MySqlPool::connect",
    "SqlitePool::connect",
    "PgPoolOptions::new",
    "MySqlPoolOptions::new",
    "SqlitePoolOptions::new",
];

/// Files allowed to call sqlx directly, with the reason.
///
/// Keep this list short and justified. A new entry is a claim that the
/// file genuinely cannot go through `Pool`; most of the time the honest
/// answer is that it can.
const ALLOWED: &[(&str, &str)] = &[
    (
        "src/sql/pool.rs",
        "the constructors themselves — this is the one place that may",
    ),
    (
        "src/tenancy/pools.rs",
        "generic over `DB: Database`; sqlx's PoolOptions<DB> has no \
         driver-specific hook, so this needs the sealed-backend trait \
         that stage 2 introduces",
    ),
    (
        "src/tenancy/database_pools.rs",
        "same generic constraint as tenancy/pools.rs",
    ),
    (
        "src/tenancy/migrate.rs",
        "schema-mode pools carry an `after_connect` hook that sets \
         `search_path`; folding that into the shared constructor is \
         stage 3",
    ),
    (
        "src/tenancy/manage/users.rs",
        "same `after_connect` search_path scoping",
    ),
    (
        "src/tenancy/manage/migrate_storage.rs",
        "same `after_connect` search_path scoping",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Every `.rs` under `src/`.
fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Is this `#[cfg(...)]` attribute gated on `test`?
///
/// Matches `test` as a whole token, so `#[cfg(all(test, feature =
/// "postgres"))]` counts and `#[cfg(feature = "testkit")]` does not.
/// Getting this wrong in the permissive direction would silently exempt
/// production code, which is the one outcome this file exists to
/// prevent.
fn is_test_gate(attr: &str) -> bool {
    let b = attr.as_bytes();
    let mut from = 0;
    while let Some(hit) = attr[from..].find("test") {
        let s = from + hit;
        let e = s + 4;
        let before_ok = s == 0 || !is_ident(b[s - 1]);
        let after_ok = e >= b.len() || !is_ident(b[e]);
        if before_ok && after_ok {
            return true;
        }
        from = e;
    }
    false
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Strip test-gated modules so fixtures are not flagged.
///
/// Brace-counting rather than parsing: it only has to find the end of a
/// module in rustfmt'd source, and the whole file is formatted by the
/// pre-commit hook. A `{` inside a string literal would fool it — a
/// false *positive* shows up as a failing test rather than a silent
/// miss, which is the right way round.
fn without_test_modules(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find("#[cfg(") {
        // Only skip the block when the gate actually names `test`;
        // otherwise emit the attribute and carry on past it.
        let attr_end = rest[at..].find(")]").map_or(rest.len(), |e| at + e + 2);
        if !is_test_gate(&rest[at..attr_end]) {
            out.push_str(&rest[..attr_end]);
            rest = &rest[attr_end..];
            continue;
        }
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(open) = after.find('{') else {
            break;
        };
        let mut depth = 0i32;
        let mut end = None;
        for (i, ch) in after[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => rest = &after[e..],
            None => break,
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn production_code_does_not_build_pools_directly() {
    let root = repo_root();
    let mut files = Vec::new();
    source_files(&root.join("src"), &mut files);
    assert!(!files.is_empty(), "found no source files to scan");

    let mut problems = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|(f, _)| *f == rel) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let production = without_test_modules(&text);
        for (idx, line) in production.lines().enumerate() {
            // Comments and doc-comments name these calls constantly when
            // explaining why not to use them.
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("*") {
                continue;
            }
            for bare in BARE {
                if line.contains(bare) {
                    problems.push(format!("{rel}:{}: `{bare}`", idx + 1));
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "{} production site(s) build a pool directly instead of through \
         `Pool::connect*`:\n  {}\n\nUse `Pool::connect`, or the typed \
         `Pool::connect_postgres` / `connect_mysql` / `connect_sqlite` when a \
         caller needs `sqlx::Pool<DB>` itself. Calling sqlx directly skips every \
         option the framework applies, which is how the main runserver pool came \
         to run on sqlx's defaults. If a site genuinely cannot, add it to ALLOWED \
         with the reason.",
        problems.len(),
        problems.join("\n  "),
    );
}

/// An allow-list entry that no longer matches a file is worse than
/// useless: it silently permits whatever gets created at that path next.
#[test]
fn every_allowed_path_still_exists() {
    let root = repo_root();
    for (rel, _why) in ALLOWED {
        assert!(
            root.join(rel).is_file(),
            "ALLOWED names `{rel}`, which does not exist — remove the entry \
             or fix the path"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::without_test_modules;

    #[test]
    fn a_test_module_is_stripped() {
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n  PgPool::connect(x);\n}\nfn b() {}";
        let out = without_test_modules(src);
        assert!(!out.contains("PgPool::connect"), "got: {out}");
        assert!(out.contains("fn a()") && out.contains("fn b()"));
    }

    #[test]
    fn nested_braces_inside_a_test_module_do_not_end_it_early() {
        let src = "#[cfg(test)]\nmod t {\n  fn f() { if x { y(); } }\n  PgPool::connect(z);\n}\nfn after() {}";
        let out = without_test_modules(src);
        assert!(!out.contains("PgPool::connect"), "got: {out}");
        assert!(out.contains("fn after()"));
    }

    #[test]
    fn production_code_outside_a_test_module_survives() {
        let src = "fn real() { PgPool::connect(u); }";
        assert!(without_test_modules(src).contains("PgPool::connect"));
    }
}
