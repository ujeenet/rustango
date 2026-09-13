//! `--help` must not describe behaviour the code does not have (#1407).
//!
//! Two lines had drifted, in opposite directions, and `docs/manage.md`
//! and `docs/mcp.md` were correct about both. So a reader who trusted
//! `--help` over the guide got the wrong answer twice — which is the
//! reverse of the usual drift and the reason this is worth pinning:
//! help text is read far more often than a guide, and nothing compiles
//! it against the parser it describes.

#![cfg(all(feature = "tenancy", feature = "manage"))]

use rustango::tenancy::manage::write_help;

fn help() -> String {
    let mut buf: Vec<u8> = Vec::new();
    write_help(&mut buf).expect("write_help");
    String::from_utf8(buf).expect("help is utf-8")
}

/// `init_tenancy_with` takes `_dir`, touches no filesystem, and returns
/// `Ok(InitTenancyReport::default())`. Help used to advertise
/// "Materialize bootstrap migrations into ./migrations/."
#[test]
fn init_tenancy_help_does_not_promise_files() {
    let h = help();
    assert!(
        h.contains("init-tenancy"),
        "the verb should still be listed — it is kept for compatibility"
    );
    assert!(
        !h.contains("Materialize bootstrap migrations"),
        "init-tenancy writes nothing; help must not say it materializes migrations:\n{h}"
    );
    assert!(
        h.contains("No-op"),
        "help should say plainly that the verb does nothing:\n{h}"
    );
}

/// The parser takes two positionals and then **flags only** — a third
/// positional is rejected with "unexpected argument". Help used to
/// advertise `create-user-key <slug> <username> [label]`.
///
/// The MCP block is feature-gated, so this only runs where the line is
/// actually emitted.
#[cfg(feature = "mcp")]
#[test]
fn create_user_key_help_shows_flags_not_a_third_positional() {
    let h = help();
    let line = h
        .lines()
        .find(|l| l.contains("create-user-key"))
        .unwrap_or_else(|| panic!("create-user-key should be listed:\n{h}"));

    assert!(
        line.contains("--label"),
        "the label is a flag, not a positional: {line}"
    );
    assert!(
        !line.contains("[label]"),
        "`[label]` reads as a third positional, which the parser rejects: {line}"
    );
}

// A third test tried to assert that every verb in the help text has a
// dispatcher arm. Dropped: the help block mixes verb entries, wrapped
// prose and `cargo run --` examples at the same indent, so a scanner
// flags "cargo", "tenant", "a" and "name" as missing verbs. Making it
// pass needed a growing list of exceptions, which is how a guard starts
// asserting its own workarounds instead of the property. If that
// property is worth holding, the help block has to become data the
// dispatcher and the printer share — not a heuristic over rendered text.
