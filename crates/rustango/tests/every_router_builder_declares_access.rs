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
        "api_version.rs",
        "VersionStrategy",
        "as_layer",
        "a layer that tags the API version; the router it wraps decides access",
    ),
    (
        "server/builder.rs",
        "Builder",
        "into_router",
        "assembles the app's own routers; each surface in it decides its access",
    ),
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
        "without an agent token only `initialize` and `ping` answer; every other method fails closed",
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
        "without an agent token only `initialize` and `ping` answer; every other method fails closed",
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

/// One `fn` item.
struct Item {
    name: String,
    /// `pub`, not `pub(crate)` or private.
    public: bool,
    /// The return type, from the arrow after the parameter list.
    ret: Option<String>,
    body: std::ops::Range<usize>,
}

/// The index just past the bracket closing the one at `open`. A `>` of
/// `->` is not a bracket.
fn close(code: &str, open: usize, (l, r): (char, char)) -> usize {
    let mut depth = 0usize;
    let mut prev = ' ';
    for (i, c) in code[open..].char_indices() {
        if c == l {
            depth += 1;
        } else if c == r && !(r == '>' && prev == '-') {
            depth -= 1;
            if depth == 0 {
                return open + i + 1;
            }
        }
        prev = c;
    }
    code.len()
}

/// `head` with its `#[...]` attributes removed.
fn without_attrs(head: &str) -> String {
    let mut out = String::new();
    let mut rest = head;
    while let Some(i) = rest.find("#[") {
        out.push_str(&rest[..i]);
        rest = &rest[close(rest, i + 1, ('[', ']')).min(rest.len())..];
    }
    out.push_str(rest);
    out
}

/// Every `fn` item with a body.
fn functions(code: &str) -> Vec<Item> {
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
        // The item's head: back to the previous line, block or statement end.
        let start = code[..p].rfind(['\n', '{', '}', ';']).map_or(0, |n| n + 1);
        let head = without_attrs(&code[start..p]);
        let public = head.split_whitespace().next() == Some("pub");
        // Generics, then the parameter list: an `Fn(..) -> X` in either is
        // not the return type.
        let mut i = at + name.len();
        if code[i..].trim_start().starts_with('<') {
            let lt = i + code[i..].find('<').unwrap();
            i = close(code, lt, ('<', '>'));
        }
        let Some(lp) = code[i..].find('(').map(|o| o + i) else {
            continue;
        };
        let after_params = close(code, lp, ('(', ')'));
        let (Some(brace), semi) = (
            code[after_params..].find('{'),
            code[after_params..].find(';'),
        ) else {
            continue;
        };
        if semi.is_some_and(|s| s < brace) {
            continue; // a trait method declaration
        }
        let open = after_params + brace;
        let tail = code[after_params..open].trim();
        let ret = tail.strip_prefix("->").map(|r| {
            let r = r.split("where").next().unwrap_or(r);
            r.trim().to_owned()
        });
        fns.push(Item {
            name,
            public,
            ret,
            body: open..block_end(code, open),
        });
    }
    fns
}

/// A type naming `Router` itself: `Router<S>`, `axum::routing::Router`,
/// `Result<Router, E>`, `(Router, T)`; not `MethodRouter`.
fn names_router(ty: &str) -> bool {
    ty.split(|c: char| !is_ident(c))
        .any(|t| t == "Router" || ROUTER_ALIASES.contains(&t))
}

/// Type aliases of `Router` in `src/`.
const ROUTER_ALIASES: &[&str] = &["ApiRouter"];

/// How a call names its callee.
#[derive(Clone, Copy, PartialEq)]
enum Via {
    /// `callee(..)`: a free fn.
    Free,
    /// `self.callee(..)` / `Self::callee(..)`: the caller's own impl.
    Own,
}

/// Whether `body` calls `callee` the way `via` says.
fn calls(body: &str, callee: &str, via: Via) -> bool {
    body.match_indices(callee).any(|(i, _)| {
        let before = &body[..i];
        let after = &body[i + callee.len()..];
        if !(after.starts_with('(') || after.starts_with("::<")) {
            return false;
        }
        let own = |p: &str| {
            before
                .strip_suffix(p)
                .is_some_and(|b| !b.chars().last().is_some_and(is_ident))
        };
        match via {
            Via::Own => own("self.") || own("Self::"),
            Via::Free => {
                !(before.chars().last().is_some_and(is_ident)
                    || before.ends_with('.')
                    || before.ends_with("::"))
            }
        }
    })
}

/// `code` with every `#[cfg(test)] mod ... { }` block blanked; what follows stays.
fn without_test_mods(mut code: String) -> String {
    let mut from = 0;
    while let Some(p) = code[from..].find("#[cfg(test)]").map(|p| p + from) {
        from = p + 1;
        let rest = code[p + "#[cfg(test)]".len()..].trim_start();
        let is_mod = rest.starts_with("mod ") || rest.starts_with("pub mod ");
        let Some(open) = code[p..].find('{').map(|o| o + p) else {
            continue;
        };
        if !is_mod || code[p..open].contains(';') {
            continue;
        }
        let end = block_end(&code, open);
        let blank: String = code[p..end]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' })
            .collect();
        code.replace_range(p..end, &blank);
    }
    code
}

/// Builders of one file per `(impl, fn)`, decided or not. Twins under
/// different `cfg`s share a key and count as decided only if all are.
fn analyze(src: &str) -> BTreeMap<(String, String), bool> {
    let code = without_test_mods(code_only(src));
    let fns: Vec<(Item, (String, bool))> = functions(&code)
        .into_iter()
        .map(|f| {
            let owner = impl_type(&code, f.body.start);
            (f, owner)
        })
        .collect();
    let body = |f: &Item| &code[f.body.clone()];
    // Same-file `(impl, fn)`s that reach the decision, to a fixed point. A
    // bare call resolves to a free fn, `self.` / `Self::` to the caller's impl.
    let reaches = |b: &str, ty: &str, decided: &BTreeSet<(&str, &str)>| {
        b.contains(DECIDED)
            || decided.iter().any(|&(dty, dname)| {
                (dty.is_empty() && calls(b, dname, Via::Free))
                    || (!dty.is_empty() && dty == ty && calls(b, dname, Via::Own))
            })
    };
    let mut decided: BTreeSet<(&str, &str)> = BTreeSet::new();
    loop {
        let next: BTreeSet<(&str, &str)> = fns
            .iter()
            .filter(|(f, (ty, _))| reaches(body(f), ty, &decided))
            .map(|(f, (ty, _))| (ty.as_str(), f.name.as_str()))
            .collect();
        if next == decided {
            break;
        }
        decided = next;
    }
    let mut found = BTreeMap::new();
    for (f, (ty, trait_impl)) in &fns {
        // A trait impl's methods are as public as the trait.
        if (f.public || *trait_impl) && f.ret.as_deref().is_some_and(names_router) {
            let ok = reaches(body(f), ty, &decided);
            *found.entry((ty.clone(), f.name.clone())).or_insert(true) &= ok;
        }
    }
    found
}

/// The type of the innermost `impl` block holding `pos` ("" outside one),
/// and whether it implements a trait.
fn impl_type(code: &str, pos: usize) -> (String, bool) {
    let mut best = (0, String::new(), false);
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
        let trait_impl = head.contains(" for ");
        let ty = head
            .split(" for ")
            .last()
            .unwrap_or("")
            .split("where")
            .next()
            .unwrap_or("");
        let ty = ty.trim().rsplit("::").next().unwrap_or("").trim();
        best = (
            p,
            ty.chars().take_while(|&c| is_ident(c)).collect(),
            trait_impl,
        );
    }
    (best.1, best.2)
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
            let src = std::fs::read_to_string(&path).expect("read");
            for ((ty, name), decided) in analyze(&src) {
                found.insert((rel.clone(), ty, name), decided);
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

/// The verdict of `(impl, fn)` in `src`; `None` when it is not a builder.
fn verdict(src: &str, ty: &str, name: &str) -> Option<bool> {
    analyze(src).get(&(ty.to_owned(), name.to_owned())).copied()
}

const DECIDED_ROUTER: &str =
    "impl Good {\npub fn router(self) -> Router { require_decided(x); r }\n}\n";

#[test]
fn another_types_same_name_call_does_not_decide() {
    let src = format!(
        "{DECIDED_ROUTER}impl Bad {{\npub fn router(self) -> Router {{ Other::new().router(x) }}\n}}"
    );
    assert_eq!(verdict(&src, "Bad", "router"), Some(false));
    assert_eq!(verdict(&src, "Good", "router"), Some(true));
}

#[test]
fn a_binding_named_like_a_decided_fn_does_not_decide() {
    let src = format!(
        "{DECIDED_ROUTER}impl Bad {{\npub fn router(self) -> Router {{ let router: Router = x; router }}\n}}"
    );
    assert_eq!(verdict(&src, "Bad", "router"), Some(false));
}

#[test]
fn own_calls_still_decide() {
    let src = format!(
        "{DECIDED_ROUTER}impl Good {{\npub fn old(self) -> Router {{ self.router() }}\n\
         pub fn again(self) -> Router {{ Self::router(self) }}\n}}"
    );
    assert_eq!(verdict(&src, "Good", "old"), Some(true));
    assert_eq!(verdict(&src, "Good", "again"), Some(true));
}

#[test]
fn code_after_a_test_module_is_scanned() {
    let src = "#[cfg(test)]\nmod tests { fn t() {} }\npub fn after() -> Router { Router::new() }";
    assert_eq!(verdict(src, "", "after"), Some(false));
}

#[test]
fn an_arrow_inside_the_parameters_is_not_the_return_type() {
    let src = "pub fn takes<F: Fn() -> Router>(f: F) -> u8 { 0 }\n\
               pub fn cb(f: impl Fn(Pool) -> Router) -> Self { x }";
    assert_eq!(verdict(src, "", "takes"), None);
    assert_eq!(verdict(src, "", "cb"), None);
}

#[test]
fn wrapped_and_qualified_routers_are_builders() {
    let src = "pub fn a() -> Result<Router, E> { x }\n\
               pub fn b() -> (Router, u8) { x }\n\
               pub fn c() -> axum::routing::Router { x }\n\
               pub fn d() -> MethodRouter { x }";
    assert_eq!(verdict(src, "", "a"), Some(false));
    assert_eq!(verdict(src, "", "b"), Some(false));
    assert_eq!(verdict(src, "", "c"), Some(false));
    assert_eq!(verdict(src, "", "d"), None);
}

#[test]
fn an_attribute_on_the_same_line_keeps_it_public() {
    let src = "#[must_use] pub fn d() -> Router { Router::new() }";
    assert_eq!(verdict(src, "", "d"), Some(false));
}

#[test]
fn cfg_twins_are_decided_only_together() {
    let src = "impl T {\n#[cfg(a)]\npub fn router(self) -> Router { r }\n\
               #[cfg(not(a))]\npub fn router(self) -> Router { require_decided(x); r }\n}";
    assert_eq!(verdict(src, "T", "router"), Some(false));
}

#[test]
fn own_calls_resolve_in_the_callers_impl() {
    let src = "impl ListView {\npub fn router(self) -> Router { require_decided(x); r }\n}\n\
               impl TemplateView {\npub fn router(self) -> Router { r }\n\
               pub fn router_at(self) -> Router { self.router() }\n}";
    assert_eq!(verdict(src, "TemplateView", "router_at"), Some(false));
    assert_eq!(verdict(src, "ListView", "router"), Some(true));
}

#[test]
fn trait_methods_and_aliases_are_builders() {
    let src = "impl Mount for App {\nfn mount(self) -> Router { r }\n}\n\
               pub fn api() -> ApiRouter { r }";
    assert_eq!(verdict(src, "App", "mount"), Some(false));
    assert_eq!(verdict(src, "", "api"), Some(false));
}
