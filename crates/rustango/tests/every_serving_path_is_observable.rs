//! Every path that serves HTTP mounts the request span and access log
//! (#1480).
//!
//! `Cli` has **five** serving paths, not one. Three go through
//! `assemble_app` — a non-Postgres build, a Postgres build on a
//! `postgres://` URL, and a multi-backend build on a non-PG URL (#560).
//! The other two are the `runserver_tenancy` variants, which never call
//! `assemble_app` at all: they hand their router to `server::Builder`.
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
//! ## The invariant, and why it changed shape
//!
//! Every call to `apply_settings_layers_or_warn` is the end of a
//! router's assembly, and each one must be followed by the mount.
//!
//! But there are **two** correct mounts, because there are two
//! topologies. On the `assemble_app` paths the router that gets layered
//! is the router that gets served, so `mount_observability(api)` is
//! right. On the tenancy paths it is not: the router is handed to
//! `server::Builder`, which merges the tenant admin into it afterwards
//! and dispatches the operator console on a sibling branch — and axum's
//! rule is that "routes added after `layer` is called will not have the
//! middleware added". Layering there covered the api router and left
//! the entire tenant-admin surface and the whole operator console
//! unlogged. Those paths pass the layer to `Builder::observability`
//! instead, which applies it to the outermost router.
//!
//! So this guard now accepts either, and adds the half it was missing:
//! that the builder actually applies what it is handed, to the
//! outermost router rather than to one branch.
//!
//! The previous `mounts >= handoffs` count is gone. It compared two
//! unrelated totals, so a path that mounted twice covered for a path
//! that never mounted at all — it could not have failed for the reason
//! its message gave.

#![cfg(feature = "config")]

fn manage_src() -> String {
    include_str!("../src/manage.rs").to_owned()
}

fn builder_src() -> String {
    include_str!("../src/server/builder.rs").to_owned()
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

/// The two topologies that correctly make a router observable: layer it
/// here, or hand it to the builder, which layers it outermost.
const DIRECT_MOUNT: &str = "mount_observability(api)";
const BUILDER_HANDOFF: &str = "server::Builder::";
const HANDS_TO_BUILDER: &str = ".observability(";

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

    // Each assembly must be followed either by the direct mount — the
    // next statement, on the `assemble_app` paths — or by a hand-off to
    // `server::Builder`, which the next test then holds to passing the
    // layer. Splitting it that way keeps both windows tight: a generous
    // one would let an unrelated later call vouch for a path.
    let mut unmounted = Vec::new();
    for &i in &settings_calls {
        let direct = lines[i..(i + 3).min(lines.len())]
            .join("\n")
            .contains(DIRECT_MOUNT);
        // The tenancy paths read env vars and open the registry pool
        // between the settings layers and the builder, so this window is
        // wider — but it only has to find the hand-off, not the layer.
        let hands_off = lines[i..(i + 20).min(lines.len())]
            .join("\n")
            .contains(BUILDER_HANDOFF);
        if !direct && !hands_off {
            unmounted.push(format!("line {}: {}", i + 1, lines[i].trim()));
        }
    }

    assert!(
        unmounted.is_empty(),
        "these serving paths assemble a router and never make it observable, so \
         requests on them log no line and handler events carry no tenant:\n\n  {}\n\n\
         Either add `let api = self.mount_observability(api);` right after the \
         settings layers (correct when the router you layer is the router you \
         serve), or pass the layer to the server builder with \
         `builder = builder.observability(layer)` (correct when you hand the router \
         to `server::Builder`, which merges and dispatches after you). There are \
         five serving paths in `Cli`, not one — three through `assemble_app` and \
         both `runserver_tenancy` variants.",
        unmounted.join("\n  ")
    );
}

/// Every hand-off to `server::Builder` passes the layer.
///
/// Anchored per hand-off rather than on a total, so a path that hands
/// off without the layer cannot be covered for by a path that does.
#[test]
fn every_builder_handoff_passes_the_layer() {
    let src = code_only(&manage_src());
    let lines: Vec<&str> = src.lines().collect();

    let handoffs: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("server::Builder::"))
        .map(|(i, _)| i)
        .collect();

    assert!(
        !handoffs.is_empty(),
        "found no `server::Builder::` construction — this guard is reading the wrong \
         text, so a pass would mean nothing"
    );

    let mut bare = Vec::new();
    for &i in &handoffs {
        // The builder is built up over a run of `builder = builder.x()`
        // lines; the layer must be handed over inside that run.
        let window = lines[i..(i + 16).min(lines.len())].join("\n");
        if !window.contains(HANDS_TO_BUILDER) {
            bare.push(format!("line {}: {}", i + 1, lines[i].trim()));
        }
    }

    assert!(
        bare.is_empty(),
        "these paths construct a `server::Builder` and never call \
         `.observability(...)` on it, so the tenant admin and the operator console \
         serve with no access log and no request span:\n\n  {}",
        bare.join("\n  ")
    );
}

/// And the builder applies what it is handed to the **outermost**
/// router.
///
/// This is the half the guard was missing. `manage.rs` could hand the
/// layer over correctly and the builder could still apply it to one
/// branch — which is the bug being fixed, one level down. The outermost
/// router is the one built from `fallback_service`, so the application
/// has to come after it.
#[test]
fn the_builder_applies_observability_to_the_outermost_router() {
    let src = code_only(&builder_src());

    let dispatch = src.find("fallback_service").expect(
        "no `fallback_service` in server/builder.rs — the Host dispatch has moved and \
         this guard can no longer tell inner from outer",
    );
    let apply = src.find(".access_log(").expect(
        "server/builder.rs never calls `.access_log(...)`, so nothing it serves is \
         logged — the tenancy paths hand it a layer it drops on the floor",
    );

    assert!(
        apply > dispatch,
        "server/builder.rs applies the access log at byte {apply}, before the Host \
         dispatch at {dispatch}. It has to go on the router built from \
         `fallback_service` — the outermost one — or it reaches only the branch it \
         was applied to, which is the bug (#1480): the tenant app carried the log \
         while the tenant admin merged in after it and the operator console on the \
         apex branch carried nothing."
    );
}
