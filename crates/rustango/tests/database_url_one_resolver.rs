//! Production code reads the database URL through one resolver.
//!
//! A raw `std::env::var("DATABASE_URL")` ignores `DB_HOST`, `DB_USER`
//! and the rest, so `manage`, `migrate` and the server each refused a
//! deployment configured with the separate vars. Test modules are exempt:
//! they read a test database's URL, not the app's configuration.

use std::path::{Path, PathBuf};

/// Raw reads of the variable that bypass `env::database_url_from_env`.
const RAW: &[&str] = &["var(\"DATABASE_URL\")", "var_os(\"DATABASE_URL\")"];

/// Files allowed a raw read, with the reason.
const ALLOWED: &[(&str, &str)] = &[
    ("src/env.rs", "the resolver itself"),
    (
        "src/testkit/matrix.rs",
        "the test matrix's Postgres URL, next to MYSQL_TEST_URL; not app config",
    ),
];

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

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

/// `test` as a whole token inside a `#[cfg(...)]`.
fn is_test_gate(attr: &str) -> bool {
    let b = attr.as_bytes();
    let mut from = 0;
    while let Some(hit) = attr[from..].find("test") {
        let s = from + hit;
        let e = s + 4;
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        if (s == 0 || !ident(b[s - 1])) && (e >= b.len() || !ident(b[e])) {
            return true;
        }
        from = e;
    }
    false
}

/// Source with every test-gated item removed (brace counting; rustfmt'd input).
fn without_test_modules(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find("#[cfg(") {
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
fn production_code_resolves_the_database_url_in_one_place() {
    let root = crate_root();
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
        for (idx, line) in without_test_modules(&text).lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            if RAW.iter().any(|raw| line.contains(raw)) {
                problems.push(format!("{rel}:{}: {}", idx + 1, line.trim()));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "raw DATABASE_URL read(s) skip the DB_* vars:\n  {}\n\nUse \
         `crate::env::database_url_from_env()`. If a site genuinely cannot, \
         add it to ALLOWED with the reason.",
        problems.join("\n  "),
    );
}

#[test]
fn every_allowed_path_still_exists() {
    let root = crate_root();
    for (rel, _why) in ALLOWED {
        assert!(root.join(rel).is_file(), "ALLOWED names missing `{rel}`");
    }
}

#[test]
fn a_test_module_is_not_scanned() {
    let src =
        "fn a() {}\n#[cfg(test)]\nmod tests {\n  std::env::var(\"DATABASE_URL\");\n}\nfn b() {}";
    let out = without_test_modules(src);
    assert!(
        !out.contains("DATABASE_URL") && out.contains("fn b()"),
        "{out}"
    );
}
