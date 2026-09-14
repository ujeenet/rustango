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
//! So this recomputes it from the source instead: find every
//! `axum::serve` and require `.with_graceful_shutdown` in the same
//! expression. Prose and call-site counting both failed here.

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
            // The serve expression runs to its terminating `.await`;
            // `with_graceful_shutdown` has to appear inside it. Bounded
            // so a *later* serve in the same file cannot vouch for this
            // one.
            let tail = &text[idx..];
            let end = tail
                .find(".await")
                .map_or(tail.len(), |e| e + ".await".len());
            if !tail[..end].contains("with_graceful_shutdown") {
                let line = text[..idx].lines().count();
                offenders.push(format!(
                    "{}:{}",
                    path.strip_prefix(&src).unwrap_or(path).display(),
                    line
                ));
            }
        }
    }

    offenders.sort();
    assert!(
        offenders.is_empty(),
        "{} bare `axum::serve` call(s) — SIGTERM kills the process outright there, so \
         anything meant to run on shutdown (a job-queue drain, a metrics flush) never \
         does, and nothing logs (#1409). Add \
         `.with_graceful_shutdown(crate::shutdown::shutdown_signal())`.\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
