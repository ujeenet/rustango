//! Every path that serves HTTP mounts the request span and access log
//! (#1480).
//!
//! `Cli` has **five** serving paths, not one. Three go through
//! `assemble_app` — a non-Postgres build, a Postgres build on a
//! `postgres://` URL, and a multi-backend build on a non-PG URL (#560).
//! The other two are the `runserver_tenancy` variants, which never call
//! `assemble_app` at all: they hand their router straight to
//! `server::Builder`.
//!
//! The first cut of the observability mount went into `assemble_app`
//! only, with a commit message calling it "the single serving path". It
//! is not. And because that change also removed the access log from
//! `apply_settings_layers`, the miss was a *regression* for exactly the
//! project shape #1480 was opened about: a multi-tenant app that called
//! `.with_settings_from_env()` had a request log before and none after.
//!
//! This is the same failure as #1457 — three copies of a serving path,
//! a fix that reached some of them — so it gets the same kind of guard
//! that one did: a structural invariant, checked as what it is.
//!
//! ## The invariant
//!
//! Every call to `apply_settings_layers_or_warn` is the end of a
//! router's assembly, and each one must be followed by
//! `mount_observability`. That is a tighter thing to check than
//! "`mount_observability` is called five times": it fails when a *new*
//! serving path is added and forgets, which is how this happens.

#![cfg(feature = "config")]

fn manage_src() -> String {
    include_str!("../src/manage.rs").to_owned()
}

/// Drop `//` comments so a search sees code, not prose about code.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_router_assembly_is_followed_by_the_observability_mount() {
    let src = code_only(&manage_src());
    let lines: Vec<&str> = src.lines().collect();

    let settings_calls: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("apply_settings_layers_or_warn(api,"))
        .map(|(i, _)| i)
        .collect();

    assert!(
        !settings_calls.is_empty(),
        "found no `apply_settings_layers_or_warn` call — this guard is reading the \
         wrong text, so a pass would mean nothing"
    );

    // Each must be followed closely by the mount. Two lines is enough:
    // the mount is the next statement in every current path, and a
    // generous window would let an unrelated later call vouch for it.
    let mut unmounted = Vec::new();
    for &i in &settings_calls {
        let window = lines[i..(i + 3).min(lines.len())].join("\n");
        if !window.contains("mount_observability(api)") {
            unmounted.push(format!("line {}: {}", i + 1, lines[i].trim()));
        }
    }

    assert!(
        unmounted.is_empty(),
        "these serving paths assemble a router and never mount the request span or \
         access log, so requests on them log no line and handler events carry no \
         tenant:\n\n  {}\n\nAdd `let api = self.mount_observability(api);` after the \
         settings layers. There are five serving paths in `Cli`, not one — three \
         through `assemble_app` and both `runserver_tenancy` variants.",
        unmounted.join("\n  ")
    );
}

/// And the mount is not quietly bypassed by a path that skips the
/// settings layers entirely.
///
/// `server::Builder` is where the tenancy paths hand off. If a future
/// path constructs one without having mounted observability first, the
/// count check above cannot see it, because it anchors on a call that
/// path never makes.
#[test]
fn every_builder_handoff_has_a_mount_before_it() {
    let src = code_only(&manage_src());
    let mounts = src.matches("mount_observability(api)").count();
    let handoffs = src.matches("server::Builder::").count();

    assert!(
        mounts >= handoffs,
        "`manage.rs` hands a router to `server::Builder` {handoffs} time(s) but calls \
         `mount_observability` only {mounts} time(s). At least one serving path \
         reaches the server without the request span or the access log."
    );
}
