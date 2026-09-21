//! There is one hardened `?next=` rule, and everything else delegates
//! to it.
//!
//! #1526 was fixed in `urls::url_has_allowed_host_and_scheme`, which
//! has no callers. The two functions that actually decide what reaches
//! a `Location` header — `tenancy::admin::sanitize_next_with_routes`
//! and `tenancy::operator_console::sanitize_next` — were hand-rolled
//! copies missing the `/\` case, so the open redirect stayed live
//! through a release that claimed to close it. Found by the review of
//! PR #1604.
//!
//! Each of those now delegates to `auth_decorators::safe_next`, and
//! each has its own unit tests against the real function. This file
//! guards the *structure*: a new hand-rolled copy is the failure mode
//! that produced the bug, and it is invisible to a test that only
//! exercises the functions it already knows about.
//!
//! Deliberately a text check. The functions are private to their
//! modules, so an integration test cannot call them; and the property
//! is "nobody wrote this predicate again", which is a property of the
//! source rather than of any value it returns.

use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// The shape of the weak check, whitespace-stripped:
/// `starts_with("//")` guarding a redirect target without the
/// backslash case beside it.
const WEAK: &str = r#".starts_with("//")"#;

/// Where the rule is allowed to live.
const CANONICAL: &str = "src/auth_decorators.rs";

/// Files that may legitimately mention the weak shape for a reason
/// other than sanitising a redirect, with that reason.
const ALLOWED: &[(&str, &str)] = &[
    (CANONICAL, "the one hardened implementation"),
    (
        "src/urls.rs",
        "the public redirect helper, hardened in #1526",
    ),
];

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

#[test]
fn no_module_hand_rolls_the_redirect_check() {
    let root = crate_root();
    let src = root.join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(
        files.len() > 50,
        "expected to walk the whole src tree, saw {} files — the walk is broken \
         and this guard would pass vacuously",
        files.len(),
    );

    let mut offenders = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|(a, _)| *a == rel) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in src.lines().enumerate() {
            let squashed = line.replace(char::is_whitespace, "");
            if squashed.contains(WEAK) {
                offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a redirect-safety check was hand-rolled again instead of calling \
         `auth_decorators::safe_next`. That predicate misses `/\\`, which a \
         browser rewrites into a protocol-relative URL — it is exactly how \
         #1526 stayed open through the release that claimed to fix it:\n  {}\n\n\
         If this is not a redirect check, add it to ALLOWED with the reason.",
        offenders.join("\n  "),
    );
}

/// The canonical rule must actually reject what every other sanitizer
/// now delegates to it for.
///
/// This used to grep the source for the literal `starts_with("/\\")`,
/// which survives *inverting the branch it guards* — the guard passed
/// 2/2 against a build where the rule was reversed (#1604 review,
/// tests-004). Calling the function is the only version of this check
/// that means anything.
#[test]
fn the_canonical_rule_rejects_what_the_copies_missed() {
    use rustango::auth_decorators::safe_next;

    for hostile in [
        // Protocol-relative, and the backslash spellings a browser
        // rewrites into it.
        "//evil.example/x",
        "/\\evil.example/x",
        "\\/evil.example/x",
        "\\\\evil.example/x",
        // Percent-encoded forms of the same.
        "/%2Fevil.example/x",
        "/%5Cevil.example/x",
        // Control characters the browser strips while parsing, which
        // turn a path-shaped value into a protocol-relative one.
        "/\u{09}/evil.example/x",
        "/\u{0d}/evil.example/x",
        "/\u{0a}/evil.example/x",
        // Absolute.
        "https://evil.example",
        "javascript:alert(1)",
    ] {
        assert_eq!(
            safe_next(hostile),
            None,
            "`{hostile}` must be refused by the canonical rule — every \
             live `?next=` sanitizer delegates here, so anything this \
             accepts reaches a `Location` header",
        );
    }
}

/// The control: tightening must not reject ordinary paths, or every
/// post-login redirect silently becomes `/`.
#[test]
fn the_canonical_rule_accepts_ordinary_paths() {
    use rustango::auth_decorators::safe_next;

    for ok in [
        "/account",
        "/a/b?q=1&r=2",
        "/x#frag",
        "/posts/hello%20world",
        "/deep/path/with/many/segments?and=query",
    ] {
        assert_eq!(
            safe_next(ok),
            Some(ok.to_owned()),
            "`{ok}` is an ordinary path and must survive",
        );
    }
}
