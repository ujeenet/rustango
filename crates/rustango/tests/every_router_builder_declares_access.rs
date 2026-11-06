//! Every public router builder makes an access decision, or is listed here
//! with the reason it needs none.
//!
//! A builder is a `pub fn` in `src/` whose signature returns a `Router`.
//! It is decided when its body, or a same-file function it calls, reaches
//! `access::require_decided`, the check that refuses to mount a model
//! surface nobody said who may use. Anything else must be in [`EXEMPT`];
//! an entry that no longer names an undecided builder fails too, so the
//! list cannot rot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The call that makes a builder decided.
const DECIDED: &str = "require_decided(";

/// `(file under src/, impl type or "", fn, why it needs no decision)`.
const EXEMPT: &[(&str, &str, &str, &str)] = &[
    (
        "admin/auth.rs",
        "",
        "protect_with_basic_auth",
        "is itself an auth layer around the caller's router",
    ),
    (
        "admin/auth.rs",
        "",
        "protect_with_csrf",
        "adds the CSRF layer; the caller's router decides access",
    ),
    (
        "admin/urls.rs",
        "Builder",
        "build",
        "the admin signs users in and checks codenames per model",
    ),
    (
        "admin/urls.rs",
        "",
        "router",
        "the admin signs users in and checks codenames per model",
    ),
    (
        "debug_panel/mod.rs",
        "",
        "debug_router",
        "development-only panel the app mounts on purpose; open by design",
    ),
    (
        "health.rs",
        "",
        "health_router",
        "liveness and readiness probes carry no data",
    ),
    (
        "health.rs",
        "HealthRouter",
        "into_router",
        "liveness and readiness probes carry no data",
    ),
    (
        "livereload.rs",
        "",
        "livereload_router",
        "serves a build version for development reloads",
    ),
    (
        "livereload.rs",
        "",
        "livereload_router_with",
        "serves a build version for development reloads",
    ),
    (
        "mcp/router.rs",
        "",
        "router",
        "documented open JSON-RPC; the `secure_*` routers check agent tokens",
    ),
    (
        "mcp/router.rs",
        "",
        "secure_tenant_router",
        "checks an agent token on every request",
    ),
    (
        "mcp/router.rs",
        "",
        "secure_tenant_router_for",
        "checks an agent token on every request",
    ),
    (
        "mcp/router.rs",
        "",
        "secure_tenant_router_from_settings",
        "checks an agent token on every request",
    ),
    (
        "mcp/router.rs",
        "",
        "secure_tenant_router_from_settings_for",
        "checks an agent token on every request",
    ),
    (
        "mcp/router.rs",
        "",
        "tenant_router",
        "documented open JSON-RPC; the `secure_*` routers check agent tokens",
    ),
    (
        "mcp/router.rs",
        "",
        "tenant_router_authed",
        "checks an agent token on every request",
    ),
    (
        "mcp/router.rs",
        "",
        "tenant_router_authed_for",
        "checks an agent token on every request",
    ),
    (
        "media/router.rs",
        "",
        "media_router",
        "refuses every request; `media_router_with` takes an authorizer",
    ),
    (
        "media/router.rs",
        "",
        "media_router_with",
        "every request goes through the caller's `MediaAuthorizer`",
    ),
    (
        "metrics.rs",
        "",
        "metrics_router",
        "serves process metrics; the app mounts it on an internal port",
    ),
    (
        "oauth2/router.rs",
        "",
        "oauth2_router",
        "the sign-in flow itself; it is how a user authenticates",
    ),
    (
        "openapi/router.rs",
        "",
        "openapi_router",
        "serves the API description, no rows",
    ),
    (
        "panic_guard.rs",
        "",
        "catch_panics",
        "wraps the caller's router; that router decides access",
    ),
    (
        "security_headers.rs",
        "",
        "csp_report_router",
        "receives browser CSP reports, which carry no credentials",
    ),
    (
        "static_files.rs",
        "",
        "static_router",
        "serves public build assets",
    ),
    (
        "template_views.rs",
        "FormView",
        "router",
        "runs the app's own handler, which owns its access",
    ),
    (
        "template_views.rs",
        "RedirectView",
        "router",
        "answers with a redirect, no rows",
    ),
    (
        "template_views.rs",
        "TemplateView",
        "router",
        "renders a fixed template, no rows",
    ),
    (
        "tenancy/admin.rs",
        "TenantAdminBuilder",
        "build",
        "the tenant admin signs users in and checks codenames",
    ),
    (
        "tenancy/auth_routes.rs",
        "JwtAuth",
        "router",
        "the token login, refresh and logout routes themselves",
    ),
    (
        "tenancy/auth_routes.rs",
        "JwtAuth",
        "router_for",
        "the token login, refresh and logout routes themselves",
    ),
    (
        "tenancy/member_auth.rs",
        "",
        "member_sso_router",
        "the member SSO sign-in flow itself",
    ),
    (
        "tenancy/member_auth.rs",
        "",
        "member_sso_router_for",
        "the member SSO sign-in flow itself",
    ),
    (
        "tenancy/operator_console/mod.rs",
        "",
        "router",
        "the operator console signs operators in on every route",
    ),
    (
        "tenancy/operator_console/mod.rs",
        "",
        "router_full",
        "the operator console signs operators in on every route",
    ),
    (
        "tenancy/operator_console/mod.rs",
        "",
        "router_with_brand_storage",
        "the operator console signs operators in on every route",
    ),
    (
        "tenancy/operator_console/mod.rs",
        "",
        "router_with_impersonation",
        "the operator console signs operators in on every route",
    ),
    (
        "tenancy/operator_console/mod.rs",
        "",
        "router_with_pools",
        "the operator console signs operators in on every route",
    ),
    (
        "tenancy/operator_console/mod.rs",
        "",
        "router_with_provisioning",
        "the operator console signs operators in on every route",
    ),
    (
        "tenancy/provision_webhook.rs",
        "",
        "router",
        "verifies the webhook signature on every request",
    ),
    (
        "testkit/mod.rs",
        "",
        "with_strict_csp",
        "test helper that wraps the caller's router",
    ),
    (
        "welcome.rs",
        "",
        "welcome_router",
        "the static welcome page of a new project",
    ),
];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// `src` with comments, strings and char literals blanked, newlines kept,
/// so a scaffold template's `pub fn router()` is not read as code.
fn code_only(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let blank = |out: &mut Vec<u8>, s: &[u8]| {
        out.extend(s.iter().map(|&c| if c == b'\n' { b'\n' } else { b' ' }));
    };
    let mut i = 0;
    while i < b.len() {
        let rest = &b[i..];
        let after_ident = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        let end = if rest.starts_with(b"//") {
            rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len())
        } else if rest.starts_with(b"/*") {
            find(rest, b"*/", 2).map_or(rest.len(), |e| e + 2)
        } else if !after_ident
            && (rest.starts_with(b"r\"") || rest.starts_with(b"r#") || rest.starts_with(b"br"))
        {
            let start = rest.iter().position(|&c| c == b'r').unwrap() + 1;
            let hashes = rest[start..].iter().take_while(|&&c| c == b'#').count();
            if rest.get(start + hashes) == Some(&b'"') {
                let mut close = vec![b'"'];
                close.extend(std::iter::repeat_n(b'#', hashes));
                find(rest, &close, start + hashes + 1).map_or(rest.len(), |e| e + close.len())
            } else {
                0
            }
        } else if rest[0] == b'"' {
            let mut j = 1;
            while j < rest.len() && rest[j] != b'"' {
                j += if rest[j] == b'\\' { 2 } else { 1 };
            }
            (j + 1).min(rest.len())
        } else if rest[0] == b'\'' {
            // A char literal, not a lifetime: `'x'` or `'\n'`, `'\u{..}'`.
            match rest.get(1) {
                Some(b'\\') => rest[2..]
                    .iter()
                    .position(|&c| c == b'\'')
                    .map_or(0, |e| e + 3),
                Some(_) if rest.get(2) == Some(&b'\'') => 3,
                _ => 0,
            }
        } else {
            0
        };
        if end == 0 {
            out.push(b[i]);
            i += 1;
        } else {
            blank(&mut out, &rest[..end]);
            i += end;
        }
    }
    String::from_utf8(out).expect("blanking keeps ASCII boundaries")
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// The index just past the `}` matching the `{` at `open`.
fn block_end(code: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (i, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + i + 1;
                }
            }
            _ => {}
        }
    }
    code.len()
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Every `fn` item: `(name, is pub, signature, body range)`.
fn functions(code: &str) -> Vec<(String, bool, String, std::ops::Range<usize>)> {
    let mut fns = Vec::new();
    let mut at = 0;
    while let Some(p) = code[at..].find("fn ").map(|p| p + at) {
        at = p + 3;
        if p > 0 && is_ident(code[..p].chars().last().unwrap()) {
            continue;
        }
        let name: String = code[at..].chars().take_while(|&c| is_ident(c)).collect();
        if name.is_empty() {
            continue;
        }
        let line_start = code[..p].rfind('\n').map_or(0, |n| n + 1);
        let head = code[line_start..p].trim();
        let is_pub = head.split_whitespace().next() == Some("pub");
        let (Some(brace), semi) = (code[at..].find('{'), code[at..].find(';')) else {
            continue;
        };
        if semi.is_some_and(|s| s < brace) {
            continue; // a trait method declaration
        }
        let open = at + brace;
        fns.push((
            name,
            is_pub,
            code[at..open].to_owned(),
            open..block_end(code, open),
        ));
    }
    fns
}

/// The type of the innermost `impl` block holding `pos`, or "".
fn impl_type(code: &str, pos: usize) -> String {
    let mut best = (0, String::new());
    let mut at = 0;
    while let Some(p) = code[at..].find("impl").map(|p| p + at) {
        at = p + 4;
        let before_ok = p == 0 || !is_ident(code[..p].chars().last().unwrap());
        let after_ok = code[at..].starts_with(['<', ' ']);
        let line_start = code[..p].rfind('\n').map_or(0, |n| n + 1);
        if !before_ok
            || !after_ok
            || !code[line_start..p].trim().is_empty()
                && !code[line_start..p].trim().starts_with("pub")
        {
            continue;
        }
        let Some(open) = code[at..].find('{').map(|o| o + at) else {
            continue;
        };
        if !(open < pos && pos < block_end(code, open)) || p < best.0 {
            continue;
        }
        let mut head = code[at..open].to_owned();
        // Drop generics, then take the type after `for`, if any.
        let mut depth = 0;
        head.retain(|c| {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => return depth == 0,
            }
            false
        });
        let ty = head
            .split(" for ")
            .last()
            .unwrap_or("")
            .split("where")
            .next()
            .unwrap_or("");
        let ty = ty.trim().rsplit("::").next().unwrap_or("").trim();
        best = (p, ty.chars().take_while(|&c| is_ident(c)).collect());
    }
    best.1
}

/// Builders per `(file, impl, fn)`, with whether each is decided.
fn builders() -> BTreeMap<(String, String, String), bool> {
    let mut found = BTreeMap::new();
    let mut stack = vec![src_root()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = path
                .strip_prefix(src_root())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let mut code = code_only(&std::fs::read_to_string(&path).expect("read"));
            // Unit tests mount whatever they like.
            if let Some(t) = code.find("#[cfg(test)]\nmod ") {
                code.truncate(t);
            }
            let fns = functions(&code);
            let bodies: Vec<(&str, &str)> = fns
                .iter()
                .map(|(n, _, _, r)| (n.as_str(), &code[r.clone()]))
                .collect();
            // Names of same-file functions that reach the decision, to a fixed
            // point. A call resolves by name; the verdict is per function.
            let reaches = |body: &str, names: &BTreeSet<&str>| {
                body.contains(DECIDED)
                    || names.iter().any(|callee| {
                        body.match_indices(callee).any(|(i, _)| {
                            let prev = body[..i].chars().last();
                            !prev.is_some_and(is_ident)
                                && body[i + callee.len()..].starts_with(['(', ':'])
                        })
                    })
            };
            let mut names: BTreeSet<&str> = BTreeSet::new();
            loop {
                let next: BTreeSet<&str> = bodies
                    .iter()
                    .filter(|(_, body)| reaches(body, &names))
                    .map(|(name, _)| *name)
                    .collect();
                if next == names {
                    break;
                }
                names = next;
            }
            for ((name, is_pub, sig, range), (_, body)) in fns.iter().zip(&bodies) {
                let returns_router = sig
                    .split("->")
                    .nth(1)
                    .is_some_and(|r| r.trim().trim_start_matches("axum::").starts_with("Router"));
                if *is_pub && returns_router {
                    let key = (rel.clone(), impl_type(&code, range.start), name.clone());
                    found.insert(key, reaches(body, &names));
                }
            }
        }
    }
    found
}

#[test]
fn every_public_router_builder_declares_access() {
    let found = builders();
    let exempt: BTreeSet<(String, String, String)> = EXEMPT
        .iter()
        .map(|(f, t, n, _)| ((*f).to_owned(), (*t).to_owned(), (*n).to_owned()))
        .collect();
    let open: Vec<_> = found
        .iter()
        .filter(|(k, decided)| !**decided && !exempt.contains(*k))
        .map(|(k, _)| k)
        .collect();
    assert!(
        open.is_empty(),
        "router builders with no access decision: {open:#?}\n\
         Route them through `access::require_decided`, or add each to EXEMPT with the reason"
    );
    let stale: Vec<_> = exempt
        .iter()
        .filter(|k| found.get(*k) != Some(&false))
        .collect();
    assert!(
        stale.is_empty(),
        "EXEMPT entries that name no undecided builder: {stale:#?}"
    );
    assert!(
        EXEMPT.iter().all(|e| !e.3.trim().is_empty()),
        "every exemption needs a reason"
    );
}

/// The model surfaces this guard exists for are seen, and decided.
#[test]
fn the_model_surfaces_are_decided() {
    let found = builders();
    for (file, ty) in [
        ("viewset/mod.rs", "ViewSet"),
        ("template_views.rs", "ListView"),
        ("template_views.rs", "DetailView"),
        ("template_views.rs", "CreateView"),
        ("template_views.rs", "UpdateView"),
        ("template_views.rs", "DeleteView"),
    ] {
        for name in ["router", "tenant_router"] {
            let key = (file.to_owned(), ty.to_owned(), name.to_owned());
            assert_eq!(found.get(&key), Some(&true), "{key:?}");
        }
    }
}

#[test]
fn the_scanner_skips_strings_and_comments() {
    let code = code_only("let s = r#\"pub fn router() -> Router {\"#; // fn x()\nfn real() {}");
    assert!(!code.contains("router"));
    assert_eq!(functions(&code).len(), 1);
}
