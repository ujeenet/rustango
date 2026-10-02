//! Every severed example crate must be built by some CI job.
//!
//! ## The gap this closes
//!
//! An example with its own `Cargo.toml` is **not** a workspace member —
//! the root `members = ["crates/*"]` does not reach it — so
//! `cargo test --workspace` never touches it. The only things that
//! build one are `doc_examples`, `platform_examples` and `e2e`, each
//! naming its example by hand.
//!
//! Two were named in neither: `mcp_demo` and `tenant_user_extension`
//! appeared in `ci.yml` only as `deny-examples` and `lockfiles` matrix
//! rows, which read a manifest and never compile the crate. So both
//! were built by nothing at all, and `tenant_user_extension`'s one
//! test — pure schema math, passes in under a second — had never run.
//!
//! Both still compiled when this was written, so nothing had rotted.
//! Nothing would have said if it had, which is the whole point.
//!
//! ## Why a manifest row does not count
//!
//! `deny-examples` runs `cargo deny check advisories` and `lockfiles`
//! runs `cargo metadata --locked`. Both take a `--manifest-path` and
//! neither compiles anything. A crate listed only there has its
//! dependencies audited while its own source is never seen by a
//! compiler — which reads like coverage and is not.
//!
//! So this guard ignores any line naming a `Cargo.toml`, and looks for
//! the two shapes that do build: a `doc_examples` matrix row
//! (`- example: <name>`) or a literal `working-directory:`.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest directory")
}

/// Directories holding a severed example crate — one with its own
/// `Cargo.toml`, and therefore outside the workspace.
///
/// A single-file example (`examples/foo.rs`, or a directory with no
/// manifest) is an `[[example]]` target of the `rustango` crate and is
/// built by `cargo test` automatically, so it needs nothing here.
fn severed_examples(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for base in ["crates/rustango/examples", "examples"] {
        let Ok(entries) = std::fs::read_dir(root.join(base)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("Cargo.toml").is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    out.push(name.to_owned());
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_severed_example_is_compiled_by_some_job() {
    let root = repo_root();
    let yaml =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("ci.yml is readable");

    // Drop manifest-path rows and comments: naming a crate in
    // `deny-examples`/`lockfiles`, or mentioning it in prose, is not
    // building it.
    let building: String = yaml
        .lines()
        .filter(|l| !l.contains("Cargo.toml"))
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    let examples = severed_examples(&root);
    assert!(
        !examples.is_empty(),
        "found no severed example crates — this guard is looking in the wrong place"
    );

    let unbuilt: Vec<&String> = examples
        .iter()
        .filter(|name| {
            let as_matrix_row = format!("- example: {name}");
            let as_workdir = format!("working-directory: crates/rustango/examples/{name}");
            let as_workdir_top = format!("working-directory: examples/{name}");
            !building.contains(&as_matrix_row)
                && !building.contains(&as_workdir)
                && !building.contains(&as_workdir_top)
        })
        .collect();

    assert!(
        unbuilt.is_empty(),
        "example crate(s) that no CI job compiles: {unbuilt:?}\n\n\
         These have their own Cargo.toml, so they are outside the workspace and \
         `cargo test --workspace` never sees them. Appearing in the `deny-examples` or \
         `lockfiles` matrix does not count — those read a manifest and compile nothing, \
         so the crate can stop building with every check green (#1592).\n\n\
         Add a `- example: <name>` row to `doc_examples`, or a job with a literal \
         `working-directory:` pointing at it."
    );
}
