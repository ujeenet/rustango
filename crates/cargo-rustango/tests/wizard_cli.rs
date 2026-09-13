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
