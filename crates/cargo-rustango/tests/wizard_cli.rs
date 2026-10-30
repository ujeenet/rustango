//! The interactive wizard must never block a non-interactive caller (#1345).
//!
//! `cargo rustango new` with no arguments opens a wizard on a terminal. Run
//! from a script, a CI job, or a `Command` with a piped stdin, the same
//! invocation must fail with a message instead of waiting forever for an
//! answer nobody is there to give.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run the scaffolder with stdin closed, returning (success, stderr).
fn run_piped(args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .args(["rustango"])
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the scaffolder");
    // Close stdin immediately: a prompt would read EOF, not hang.
    drop(child.stdin.take().expect("stdin").write_all(b""));
    let out = child.wait_with_output().expect("wait");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn bare_new_without_a_terminal_asks_for_a_name() {
    let (ok, err) = run_piped(&["new"]);
    assert!(!ok, "should not have scaffolded anything: {err}");
    assert!(
        err.contains("missing project name"),
        "expected the non-interactive error, got: {err}"
    );
}

#[test]
fn interactive_without_a_terminal_says_so() {
    let (ok, err) = run_piped(&["new", "--interactive"]);
    assert!(!ok, "{err}");
    assert!(
        err.contains("needs a terminal"),
        "expected a clear refusal, got: {err}"
    );
}

/// `-i` alongside a full set of flags still refuses rather than silently
/// falling back — the user asked for prompts and cannot have them.
#[test]
fn interactive_with_flags_still_refuses_without_a_terminal() {
    let (ok, err) = run_piped(&["new", "probe", "-i", "--backend", "sqlite"]);
    assert!(!ok, "{err}");
    assert!(err.contains("needs a terminal"), "{err}");
}

/// The flags path must stay non-interactive: fully specified, stdin closed,
/// it scaffolds without asking anything.
#[test]
fn fully_specified_flags_never_prompt() {
    let work = std::env::temp_dir().join(format!("rustango-wizard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("scratch");
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .args([
            "rustango",
            "new",
            "probe",
            "--template",
            "api",
            "--backend",
            "sqlite",
        ])
        .current_dir(&work)
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(work.join("probe").join("Cargo.toml").is_file());
    let _ = std::fs::remove_dir_all(&work);
}

/// `cargo rgo` is the same command: cargo runs `cargo-rgo rgo …`.
#[test]
fn cargo_rgo_runs_the_scaffolder() {
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-rgo"))
        .args(["rgo", "new"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .output()
        .expect("run cargo-rgo");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("missing project name"), "{err}");
}

/// `--rustango-path` is typed from the shell, so a relative path is read
/// from the caller's directory, not the new project's; a path with no
/// `Cargo.toml` is refused before anything is written.
#[test]
fn a_relative_rustango_path_is_read_from_the_callers_directory() {
    let work = std::env::temp_dir().join(format!("rustango-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("checkout")).expect("scratch");
    std::fs::write(work.join("checkout").join("Cargo.toml"), "").expect("manifest");
    let new = |name: &str, path: &str| {
        Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
            .args(["rustango", "new", name, "--backend", "sqlite"])
            .args(["--rustango-path", path])
            .current_dir(&work)
            .stdin(Stdio::null())
            .output()
            .expect("run")
    };

    let out = new("demo", "checkout");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = std::fs::read_to_string(work.join("demo").join("Cargo.toml")).expect("read");
    let doc: toml::Table = toml::from_str(&manifest).expect("parses");
    let dep = doc["dependencies"]["rustango"]["path"]
        .as_str()
        .expect("path dep");
    // Cargo reads a relative path from the project folder.
    assert!(
        work.join("demo").join(dep).join("Cargo.toml").is_file(),
        "{dep} does not reach the checkout"
    );

    let out = new("other", "missing");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("Cargo.toml"), "{err}");
    assert!(!work.join("other").exists(), "scaffolded anyway");
    let _ = std::fs::remove_dir_all(&work);
}
