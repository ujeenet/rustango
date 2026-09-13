//! `--help` must not describe behaviour the code does not have (#1407).
//!
//! Two lines had drifted, in opposite directions, and `docs/manage.md`
//! and `docs/mcp.md` were correct about both. So a reader who trusted
//! `--help` over the guide got the wrong answer twice — which is the
//! reverse of the usual drift and the reason this is worth pinning:
//! help text is read far more often than a guide, and nothing compiles
//! it against the parser it describes.
//!
//! ## Required features
//!
//! The real assertions need `tenancy,manage`, and the `create-user-key`
//! one also needs `mcp` — that help block is gated, so the line it
//! checks is not emitted without it.
//!
//! A gated test file reports `running 0 tests … ok` when the features
//! are off, which is a green that asserted nothing. Someone editing help
//! strings runs exactly this file, so that green is worst where it is
//! most likely. `feature_summary` below is always compiled, so the run
//! always names what did and did not happen instead of being silent.
//!
//! It does not *fail* when the features are off: `manage` is in
//! `batteries` but `tenancy` is not, so a plain `cargo test -p rustango`
//! has one and not the other, and failing there would break the most
//! common local command to report a configuration that is fine.

#[cfg(all(feature = "tenancy", feature = "manage"))]
use rustango::tenancy::manage::write_help;

/// Always compiled. Names the feature set so a run that asserts nothing
/// says so, rather than printing `0 tests … ok`.
#[test]
fn feature_summary() {
    let gated = cfg!(all(feature = "tenancy", feature = "manage"));
    let mcp = cfg!(feature = "mcp");
    println!(
        "help guard: tenancy+manage = {gated}, mcp = {mcp} — \
         {} assertion(s) active",
        usize::from(gated) + usize::from(gated && mcp)
    );
    assert!(
        !(mcp && !gated),
        "`mcp` without `tenancy,manage` cannot happen — if it does, the \
         cfg on this file is wrong and the guard is silently inert"
    );
}

#[cfg(all(feature = "tenancy", feature = "manage"))]
fn help() -> String {
    let mut buf: Vec<u8> = Vec::new();
    write_help(&mut buf).expect("write_help");
    String::from_utf8(buf).expect("help is utf-8")
}

/// `init_tenancy_with` takes `_dir`, touches no filesystem, and returns
/// `Ok(InitTenancyReport::default())`. Help used to advertise
/// "Materialize bootstrap migrations into ./migrations/."
#[cfg(all(feature = "tenancy", feature = "manage"))]
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
#[cfg(all(feature = "tenancy", feature = "manage", feature = "mcp"))]
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
