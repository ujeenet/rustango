//! The test this crate never had (#1207).
//!
//! `templates.rs` claimed "CI snapshot-tests the generated output by running
//! `cargo check` on each template". Nothing did — the unit tests are
//! `.contains()` assertions on template *strings*, and no test ever wrote a
//! project to disk. That is how `--template api` came to generate a project
//! that failed with 75 compiler errors, and how two templates came to emit
//! warnings, without anything going red.
//!
//! These tests generate each template into a tempdir and compile it. They use
//! `--rustango-path` so they validate the **working tree** rather than the last
//! published release — checking against crates.io would have said "fine" while
//! the tree was broken, and vice versa.
//!
//! Marked `#[ignore]` because each case compiles rustango (minutes, not
//! milliseconds) and `cargo test` should stay fast for everyone else. CI runs
//! them explicitly:
//!
//! ```sh
//! cargo test -p cargo-rustango -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to `crates/rustango` in this checkout.
fn rustango_crate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("rustango")
}

/// A unique scratch directory. No `tempfile` dep — this crate deliberately has
/// zero dependencies, and the scaffolder must keep working without them.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustango-scaffold-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Generate `template` into a fresh directory and return the project root.
fn generate(template: &str, tag: &str) -> (PathBuf, PathBuf) {
    let work = scratch(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .current_dir(&work)
        .args([
            "rustango",
            "new",
            "probe",
            "--template",
            template,
            "--rustango-path",
        ])
        .arg(rustango_crate())
        .output()
        .expect("run the scaffolder");
    assert!(
        out.status.success(),
        "scaffolding {template} failed:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let root = work.join("probe");
    (work, root)
}

/// `cargo check` the generated project, returning (compiled, warnings, log).
///
/// Warnings are counted **only for the generated crate**, never for rustango.
/// The obvious implementation — `RUSTFLAGS=-D warnings` — is wrong: RUSTFLAGS
/// applies to every crate in the graph, so the framework's own warnings would
/// fail the test and mask the thing being measured. `--message-format=json`
/// carries the emitting package on each diagnostic, so filter on that.
///
/// The bar matters because a project you have just generated is the one moment
/// when there is nothing of yours to blame: anything the compiler says about it
/// is the generator's (#1210).
fn check(root: &Path, extra: &[&str]) -> (bool, usize, String) {
    let out = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["check", "--message-format=json-diagnostic-rendered-ansi"])
        .args(extra)
        // One shared target dir across every case, so rustango compiles once
        // per feature set rather than once per test.
        .env(
            "CARGO_TARGET_DIR",
            std::env::temp_dir().join("rustango-scaffold-target"),
        )
        .env("CARGO_INCREMENTAL", "0")
        .output()
        .expect("run cargo check");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut warnings = Vec::new();
    for line in stdout.lines() {
        // Crude but dependency-free JSON probing — this crate has no deps by
        // design. A compiler-message line for the probe package looks like:
        //   {"reason":"compiler-message","package_id":"…probe…","message":{…}}
        if !line.contains(r#""reason":"compiler-message""#) {
            continue;
        }
        if !line.contains("probe") {
            continue; // a dependency's diagnostic — not ours to judge
        }
        if line.contains(r#""level":"warning""#) {
            warnings.push(line.to_owned());
        }
    }
    let log = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
    (out.status.success(), warnings.len(), log)
}

fn assert_compiles(template: &str, extra: &[&str], label: &str) {
    let (work, root) = generate(template, label);
    let (ok, warnings, log) = check(&root, extra);
    let _ = std::fs::remove_dir_all(&work);
    assert!(ok, "`{template}` {label} did not compile:\n{log}");
    assert_eq!(
        warnings, 0,
        "`{template}` {label} compiled but emitted {warnings} warning(s) — a \
         freshly generated project must be clean (#1210):\n{log}"
    );
}

// ---------------------------------------------------------------- as generated

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn api_template_compiles() {
    assert_compiles("api", &[], "default");
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn fullstack_template_compiles() {
    assert_compiles("fullstack", &[], "default");
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenant_template_compiles() {
    assert_compiles("tenant", &[], "default");
}

// ------------------------------------------------- the advertised backend switch

// The generated manifest documents `cargo run --no-default-features --features
// sqlite`. That invocation used to be the one that *broke* the build: the
// dependency pinned `rustango/postgres` while the project's own `postgres`
// feature was off, and `#[derive(Model)]` gates its emissions on the project's
// features — so rustango demanded `MaybePgFromRow` and the derive had skipped
// it (#1211).

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn api_template_compiles_on_sqlite() {
    assert_compiles(
        "api",
        &["--no-default-features", "--features", "sqlite"],
        "sqlite",
    );
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn api_template_compiles_on_mysql() {
    assert_compiles(
        "api",
        &["--no-default-features", "--features", "mysql"],
        "mysql",
    );
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn fullstack_template_compiles_on_sqlite() {
    assert_compiles(
        "fullstack",
        &["--no-default-features", "--features", "sqlite"],
        "sqlite",
    );
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenant_template_compiles_on_sqlite() {
    assert_compiles(
        "tenant",
        &["--no-default-features", "--features", "sqlite"],
        "sqlite",
    );
}
