//! Every `axum::serve` in the crate installs graceful shutdown (#1409).
//!
//! The first pass at #1409 added `Cli::on_shutdown` and wired it into
//! five call sites — and three of them worked. The other two sat behind
//! `Builder::serve`, which was a bare `axum::serve(..).await?`. SIGTERM
//! hit the default disposition there, the process died, and the
//! `run_shutdown_hook` on the very next line never ran.
//!
//! The reasoning that let it through is worth recording, because it is
//! the same mistake the rest of this release is about. I verified the
//! hook was **called** at five sites, not that it was **reachable** at
//! five sites. Counting call sites is a proxy for "the shutdown path
//! works"; the property is whether the serve underneath ever returns.
//!
//! A commit comment even asserted "the tenancy server drains via its own
//! `shutdown_signal`" — true of `tenancy::server::run`, which is the
//! `manage server` subcommand, and not of the path `Cli::run` actually
//! takes. Close enough to be convincing, and wrong.
//!
//! `shutdown::serve_until_drained` now takes the listener and router and
//! is the one `axum::serve` in the crate, so any other serve is a bypass
//! of the drain, even one with its own `with_graceful_shutdown` (#1948).

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_bare_axum_serve_in_the_crate() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let mut offenders = Vec::new();
    for path in &files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        // Only real serve paths. A doc comment teaching `axum::serve` is
        // prose, and a throwaway server spawned inside `#[cfg(test)]`
        // has nothing to drain — neither is what leaves a job queue
        // dying on deploy.
        let test_mod = text.find("#[cfg(test)]").unwrap_or(text.len());
        for (idx, _) in text.match_indices("axum::serve(") {
            if idx > test_mod {
                continue;
            }
            let line_start = text[..idx].rfind('\n').map_or(0, |n| n + 1);
            let line_prefix = text[line_start..idx].trim_start();
            if line_prefix.starts_with("//") {
                continue;
            }
            let rel = path.strip_prefix(&src).unwrap_or(path);
            // The wrapper itself, and the test harness that stops on its own signal.
            if rel == Path::new("shutdown.rs") || rel == Path::new("test_server.rs") {
                continue;
            }
            let line = text[..idx].lines().count();
            offenders.push(format!("{}:{}", rel.display(), line));
        }
    }

    offenders.sort();
    assert!(
        offenders.is_empty(),
        "{} `axum::serve` call(s) outside `shutdown::serve_until_drained` — they skip \
         the drain deadline or SIGTERM handling (#1409, #1948). Call the wrapper.\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
