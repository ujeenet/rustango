//! Every feature a template names must exist in the `rustango` it pins (#1217).
//!
//! The generated manifest pins `rustango` by **version** — the published crate —
//! while the template's feature list is written against the tree in this repo.
//! When those disagree, a scaffolded project fails at *resolution*, before a
//! single line compiles:
//!
//! ```text
//! error: failed to select a version for `rustango`.
//! package `myblog` depends on `rustango` with feature `batteries`
//! but `rustango` does not have that feature.
//! ```
//!
//! That is how the `batteries` feature broke 36 of 50 generated projects
//! (every `fullstack` and `tenant`) the moment it was named by a template but
//! not yet released.
//!
//! `generated_project_compiles.rs` cannot catch this class: it builds with
//! `--rustango-path`, deliberately, so it validates the working tree rather
//! than the last release — which is right for *that* test and blind to *this*
//! failure.
//!
//! Checking against the live registry would trade one blind spot for another
//! (it can only ever describe the last release, and it needs the network). So
//! this asserts the invariant directly and offline: **the scaffolder may only
//! name features the framework in this repo actually defines.** Combined with
//! publishing all four crates from one version in lockstep — `rustango` before
//! `cargo-rustango`, always — a named feature is guaranteed to exist in the
//! version the template pins.

use std::path::{Path, PathBuf};

fn rustango_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("rustango")
        .join("Cargo.toml")
}

/// Feature names defined in `crates/rustango/Cargo.toml`.
///
/// Deliberately a hand-rolled scan rather than a TOML dependency: this crate
/// ships with zero dependencies and the scaffolder must keep working without
/// any, so the test suite holds the same line. A feature definition is a line
/// of the form `name = [` at column zero inside `[features]`.
fn framework_features(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_features = false;
    for line in manifest.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
            continue;
        }
        if !in_features {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.starts_with('#') || !rest.trim_start().starts_with('[') {
            continue;
        }
        out.push(name.to_owned());
    }
    out
}

/// Feature names a generated `Cargo.toml` asks of `rustango`.
///
/// Two shapes matter, and both are the project's *request* of the framework:
///   - the dependency's own list:  `rustango = { …, features = ["batteries"] }`
///   - the forwarding features:    `postgres = ["rustango/postgres"]`
fn requested_features(generated: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in generated.lines() {
        let t = line.trim();
        if t.starts_with("rustango = ") {
            if let Some(idx) = t.find("features = [") {
                let tail = &t[idx + "features = [".len()..];
                if let Some(end) = tail.find(']') {
                    for f in tail[..end].split(',') {
                        let f = f.trim().trim_matches('"').trim();
                        if !f.is_empty() {
                            out.push(f.to_owned());
                        }
                    }
                }
            }
        }
        // `sqlite = ["rustango/sqlite"]` — the forwarded backend.
        for part in t.split('"') {
            if let Some(f) = part.strip_prefix("rustango/") {
                out.push(f.to_owned());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn generate_manifest(template: &str) -> String {
    let work = std::env::temp_dir().join(format!(
        "rustango-featcheck-{}-{template}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&work).expect("scratch dir");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .current_dir(&work)
        .args(["rustango", "new", "probe", "--template", template])
        .output()
        .expect("run the scaffolder");
    assert!(
        out.status.success(),
        "scaffolding {template}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest =
        std::fs::read_to_string(work.join("probe").join("Cargo.toml")).expect("read manifest");
    let _ = std::fs::remove_dir_all(&work);
    manifest
}

#[test]
fn every_template_names_only_features_the_framework_defines() {
    let manifest_path = rustango_manifest();
    if !manifest_path.is_file() {
        eprintln!("skipping: crates/rustango not alongside — not a repo checkout");
        return;
    }
    let defined = framework_features(&std::fs::read_to_string(&manifest_path).expect("read"));
    assert!(
        defined.contains(&"manage".to_owned()),
        "sanity: the feature scan found nothing useful (got {} names)",
        defined.len()
    );

    for template in ["api", "fullstack", "tenant"] {
        let generated = generate_manifest(template);
        let requested = requested_features(&generated);
        assert!(
            !requested.is_empty(),
            "`{template}` requested no rustango features — the scan is wrong"
        );
        for feat in &requested {
            assert!(
                defined.contains(feat),
                "template `{template}` requests rustango feature `{feat}`, which \
                 crates/rustango/Cargo.toml does not define.\n\
                 A generated project pins the PUBLISHED rustango, so naming a \
                 feature that does not exist there fails at resolution before \
                 anything compiles (#1217).\n\
                 Defined features: {defined:?}"
            );
        }
    }
}

/// The backend forwards are the ones most likely to be typo'd, since they are
/// spelled twice — once in `[features]`, once as `rustango/<name>`.
#[test]
fn backend_forwards_resolve() {
    if !rustango_manifest().is_file() {
        return;
    }
    let generated = generate_manifest("fullstack");
    for backend in ["postgres", "sqlite", "mysql"] {
        assert!(
            generated.contains(&format!(r#"{backend} = ["rustango/{backend}"]"#)),
            "generated manifest is missing the `{backend}` forward:\n{generated}"
        );
    }
}
