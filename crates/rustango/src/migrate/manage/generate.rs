//! The `make:*` code generators.

use super::*;

// ============================================================ make:* generators

/// What shape of name a `make:*` generator accepts.
///
/// Most emit a **type** (`make:viewset Widget` → `struct WidgetViewSet`), so
/// PascalCase is the right rule. `make:test` emits a **file of test functions**
/// — `tests/post_smoke.rs` — where snake_case is the Rust convention and the
/// name `docs/manage.md` itself documents (#1214).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NameShape {
    /// PascalCase, e.g. `PostViewSet`.
    Type,
    /// Any Rust identifier: `post_smoke` and `PostSmoke` both accepted.
    Identifier,
}

pub(super) fn parse_name_and_model(
    args: &[String],
) -> Result<(String, Option<String>, String), MigrateError> {
    parse_name_and_model_as(args, NameShape::Type)
}

pub(super) fn parse_name_and_model_as(
    args: &[String],
    shape: NameShape,
) -> Result<(String, Option<String>, String), MigrateError> {
    let mut name: Option<String> = None;
    let mut model: Option<String> = None;
    // Phase 2b of #145 — every `make:*` scaffolder emits `use rustango::…`
    // in its template. A `rustango-orm` consumer (post-#144) needs the
    // emit to read `use rustango_orm::…` instead. Default stays
    // `"rustango"` so today's invocations stay bit-identical.
    let mut crate_root: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--model" => {
                model = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| missing_value("--model"))?,
                );
            }
            "--crate" => {
                let v = iter
                    .next()
                    .cloned()
                    .ok_or_else(|| missing_value("--crate"))?;
                let trimmed = v.trim();
                if trimmed.is_empty() {
                    return Err(MigrateError::Validation(
                        "--crate <name> must not be empty".into(),
                    ));
                }
                crate_root = Some(trimmed.to_owned());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag `{other}`")));
            }
            other => {
                if name.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected positional `{other}`"
                    )));
                }
                name = Some(other.to_owned());
            }
        }
    }
    let name = name.ok_or_else(|| {
        MigrateError::Validation("name is required (e.g. `manage make:viewset PostViewSet`)".into())
    })?;
    match shape {
        NameShape::Type if !is_valid_type_name(&name) => {
            return Err(MigrateError::Validation(format!(
                "`{name}` is not a valid Rust type name (PascalCase, alphanumeric + underscore)"
            )));
        }
        NameShape::Identifier if !is_valid_identifier(&name) => {
            return Err(MigrateError::Validation(format!(
                "`{name}` is not a valid Rust identifier (letters, digits, underscore; \
                 must not start with a digit)"
            )));
        }
        _ => {}
    }
    // The file becomes `pub mod <snake>;`: `pub mod type;` does not parse (#1913).
    let module = pascal_to_snake(&name);
    if name == "Self" || is_reserved_module_name(&module) {
        return Err(MigrateError::Validation(format!(
            "`{name}` would make the module `{module}`, which is a Rust keyword or \
             shadows a built-in crate — pick another name"
        )));
    }
    Ok((name, model, crate_root.unwrap_or_else(|| "rustango".into())))
}

/// Keywords (2015–2024, strict and reserved) plus the crate names a module
/// may not shadow. `cargo-rustango` keeps its own copy; it links no rustango.
pub(in crate::migrate) fn is_reserved_module_name(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "abstract",
        "alloc",
        "as",
        "async",
        "await",
        "become",
        "box",
        "break",
        "const",
        "continue",
        "core",
        "crate",
        "do",
        "dyn",
        "else",
        "enum",
        "extern",
        "false",
        "final",
        "fn",
        "for",
        "gen",
        "if",
        "impl",
        "in",
        "let",
        "loop",
        "macro",
        "match",
        "mod",
        "move",
        "mut",
        "override",
        "priv",
        "proc_macro",
        "pub",
        "ref",
        "return",
        "self",
        "static",
        "std",
        "struct",
        "super",
        "trait",
        "true",
        "try",
        "type",
        "typeof",
        "unsafe",
        "unsized",
        "use",
        "virtual",
        "where",
        "while",
        "yield",
    ];
    RESERVED.contains(&name)
}

pub(super) fn is_valid_type_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// A Rust identifier — any case. Used by `make:test`, whose output is a file of
/// `#[test] fn`s rather than a type.
pub(super) fn is_valid_identifier(name: &str) -> bool {
    crate::core::is_plain_identifier(name)
}

pub(super) fn pascal_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Write a generated source file. A project that renamed the dependency
/// (`rgo = { package = "rustango", .. }`) gets its name in the code.
pub(super) fn write_source(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    match crate::migrate::scaffold::renamed_dep(std::path::Path::new(".")) {
        Some(to) => std::fs::write(path, crate::migrate::scaffold::rename_dep(body, &to)),
        None => std::fs::write(path, body),
    }
}

pub(super) fn write_generated<W: Write>(
    w: &mut W,
    file_name: &str,
    contents: String,
) -> Result<(), MigrateError> {
    let path = std::path::PathBuf::from("src").join(file_name);
    refuse_overwrite(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_source(&path, &contents)?;
    writeln!(w, "wrote {}", path.display())?;
    // Lead with whichever entry the project actually has. A scaffolded
    // project keeps its modules in the library so that binaries under
    // `src/bin/` can reach them — `mod` there would hide it from them.
    let module = file_name.trim_end_matches(".rs");
    if std::path::Path::new("src/lib.rs").exists() {
        writeln!(w, "  add `pub mod {module};` to src/lib.rs")?;
    } else {
        writeln!(
            w,
            "  add `mod {module};` to src/main.rs (or `pub mod ...;` to src/lib.rs)"
        )?;
    }
    Ok(())
}

pub(super) const MAKE_VERBS: &[&str] = &[
    "make:viewset",
    "make:api_routes",
    "make:serializer",
    "make:form",
    "make:job",
    "make:scheduled",
    "make:worker",
    "make:notification",
    "make:middleware",
    "make:test",
];

/// Run a `make:*` verb, refusing one whose output needs a rustango feature
/// this build lacks: the file would not compile.
pub(super) fn make_cmd<W: Write>(
    cmd: &str,
    args: &[String],
    w: &mut W,
    has_feature: impl Fn(&str) -> bool,
) -> Result<(), MigrateError> {
    if let Some(feature) = missing_make_feature(cmd, &has_feature) {
        return Err(missing_feature_error(cmd, feature));
    }
    match cmd {
        "make:viewset" => make_viewset_cmd(args, w, has_feature),
        "make:api_routes" => make_api_routes_cmd(args, w),
        "make:serializer" => make_serializer_cmd(args, w),
        "make:form" => make_form_cmd(args, w),
        "make:job" => make_job_cmd(args, w),
        "make:scheduled" => make_scheduled_cmd(args, w),
        "make:worker" => make_worker_cmd(args, w),
        "make:notification" => make_notification_cmd(args, w),
        "make:middleware" => make_middleware_cmd(args, w),
        "make:test" => make_test_cmd(args, w),
        other => Err(MigrateError::Validation(format!("unknown verb `{other}`"))),
    }
}

pub(super) fn missing_feature_error(cmd: &str, feature: &str) -> MigrateError {
    MigrateError::Validation(format!(
        "{cmd}: the code it writes needs rustango's `{feature}` feature, which this \
         project does not enable; add it to the rustango dependency's features"
    ))
}

/// The feature a verb's output needs, when `has_feature` says it is off.
/// `make:viewset` checks its own, once it knows which template it writes.
pub(super) fn missing_make_feature(
    cmd: &str,
    has_feature: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    let feature = match cmd {
        // `make:test` writes a `test_client` test, which is `admin`-gated.
        "make:test" => "admin",
        "make:serializer" => "serializer",
        "make:form" => "forms",
        "make:job" => "jobs",
        // The worker drains the database queue, which `jobs` alone lacks.
        "make:worker" => "jobs-postgres",
        "make:scheduled" => "scheduler",
        "make:notification" => "notifications",
        _ => return None,
    };
    (!has_feature(feature)).then_some(feature)
}

/// Whether this build of rustango, i.e. the project's, has `feature`.
pub(super) fn compiled_feature(feature: &str) -> bool {
    match feature {
        "admin" => cfg!(feature = "admin"),
        "tenancy" => cfg!(feature = "tenancy"),
        "serializer" => cfg!(feature = "serializer"),
        "forms" => cfg!(feature = "forms"),
        "jobs" => cfg!(feature = "jobs"),
        "jobs-postgres" => cfg!(feature = "jobs-postgres"),
        "scheduler" => cfg!(feature = "scheduler"),
        "notifications" => cfg!(feature = "notifications"),
        _ => false,
    }
}

/// Where a `--model` lives: `use {path};` imports it, `{name}` names it.
pub(super) struct ModelRef {
    pub(super) path: String,
    pub(super) name: String,
}

impl ModelRef {
    pub(super) fn bare(name: &str) -> Self {
        Self {
            path: format!("crate::models::{name}"),
            name: name.to_owned(),
        }
    }

    /// `blog::models::Post` is taken as written; a bare `Post` is looked up
    /// in `src/models.rs`, then in each `src/<app>/models.rs`.
    pub(super) fn resolve(arg: &str, src: &std::path::Path) -> Self {
        let rel = arg.strip_prefix("crate::").unwrap_or(arg);
        if let Some((_, name)) = rel.rsplit_once("::") {
            return Self {
                path: format!("crate::{rel}"),
                name: name.to_owned(),
            };
        }
        let declares = |dir: &std::path::Path| {
            [dir.join("models.rs"), dir.join("models").join("mod.rs")]
                .iter()
                .filter_map(|f| std::fs::read_to_string(f).ok())
                .any(|text| declares_struct(&text, arg))
        };
        if declares(src) {
            return Self::bare(arg);
        }
        let mut apps: Vec<std::path::PathBuf> = std::fs::read_dir(src)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        apps.sort();
        for app in apps {
            let Some(app_name) = app.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if declares(&app) {
                return Self {
                    path: format!("crate::{app_name}::models::{arg}"),
                    name: arg.to_owned(),
                };
            }
        }
        Self::bare(arg)
    }
}

/// Whether `text` has a line `pub struct {name}` (then a non-ident char).
pub(super) fn declares_struct(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("pub struct ")
            .and_then(|rest| rest.strip_prefix(name))
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}

pub(super) fn make_viewset_cmd<W: Write>(
    args: &[String],
    w: &mut W,
    has_feature: impl Fn(&str) -> bool,
) -> Result<(), MigrateError> {
    // Tenancy projects need a different scaffold shape — the
    // single-pool `#[derive(ViewSet)]` is fatal for them (#80) — so
    // this command toggles between two templates. Resolution order:
    //
    //   1. `--no-tenant` flag → pool template (escape hatch when
    //      auto-detection guesses wrong, or when a tenancy project
    //      hand-rolls a single-pool viewset for some reason)
    //   2. `--tenant` flag → tenant template (explicit)
    //   3. Cargo.toml has `features = [..., "tenancy", ...]` on the
    //      `rustango` dep → tenant template (auto-detected)
    //   4. Otherwise → pool template
    //
    // The auto-detect path means you do not need a flag for the
    // obvious thing: tenancy projects get
    // `tenant_router` without the user having to remember `--tenant`.
    let mut explicit_tenant = false;
    let mut explicit_no_tenant = false;
    let mut filtered: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        if a == "--tenant" || a == "--tenant-aware" {
            explicit_tenant = true;
        } else if a == "--no-tenant" {
            explicit_no_tenant = true;
        } else {
            filtered.push(a.clone());
        }
    }
    let (tenant_aware, echoed_auto_detect) =
        resolve_viewset_tenant_mode(explicit_tenant, explicit_no_tenant, project_uses_tenancy());
    // Echo when auto-detection picked tenant mode — silent
    // auto-config surprises users; one informational line points
    // them at the override flag without being noisy.
    if echoed_auto_detect {
        writeln!(
            w,
            "make:viewset: auto-detected tenancy mode from Cargo.toml (pass `--no-tenant` to override)"
        )?;
    }
    // The tenant template uses `ViewSet`, which `tenancy` alone provides.
    let (verb, feature) = if tenant_aware {
        ("make:viewset --tenant", "tenancy")
    } else {
        ("make:viewset", "admin")
    };
    if !has_feature(feature) {
        return Err(missing_feature_error(verb, feature));
    }
    let (name, model, crate_root) = parse_name_and_model(&filtered)?;
    let snake = pascal_to_snake(&name);
    let model = ModelRef::resolve(
        model.as_deref().unwrap_or("Post"),
        std::path::Path::new("src"),
    );
    let app_crate = project_crate_name();
    let (route, registered) = viewset_route(&model, &app_crate, registered_models());
    let body = if tenant_aware {
        viewset_template_tenant(&name, &model, &snake, &route, &crate_root)
    } else {
        viewset_template_pool(
            &name,
            &model,
            &snake,
            &route,
            &crate_root,
            &app_crate,
            has_feature("tenancy"),
        )
    };
    write_generated(w, &format!("{snake}.rs"), body)?;
    writeln!(
        w,
        "  answers at /api{route} after you mount it as the end of the file shows"
    )?;
    if !registered {
        writeln!(
            w,
            "  `{}` is not compiled in yet, so the route is guessed from its name; use its `table` if it sets one",
            model.name
        )?;
    }
    Ok(())
}

/// `(module_path, name, table)` of every model compiled into this binary.
pub(super) fn registered_models() -> Vec<(&'static str, &'static str, &'static str)> {
    inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .map(|e| (e.module_path, e.schema.name, e.schema.table))
        .collect()
}

/// The path a ViewSet over `model` serves under `/api`: its registered table,
/// else a guess from its name (`false` flags the guess).
pub(super) fn viewset_route<'a>(
    model: &ModelRef,
    app_crate: &str,
    entries: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> (String, bool) {
    match registered_table(model, app_crate, entries) {
        Some(table) => (format!("/{table}"), true),
        None => (format!("/{}", pascal_to_snake(&model.name)), false),
    }
}

/// The table of the project's model `model`, from the registered inventory
/// of `(module_path, name, table)`. The ViewSet route follows it, as the
/// admin does. Framework models of the same name are not the project's.
pub(super) fn registered_table<'a>(
    model: &ModelRef,
    app_crate: &str,
    entries: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> Option<&'a str> {
    let rel = model.path.strip_prefix("crate::").unwrap_or(&model.path);
    let module = rel.rsplit_once("::").map_or("", |(m, _)| m);
    let own: Vec<(&str, &str)> = entries
        .into_iter()
        .filter(|(_, name, _)| *name == model.name)
        .filter_map(|(path, _, table)| {
            let rest = path.strip_prefix(app_crate)?;
            let module = if rest.is_empty() {
                ""
            } else {
                rest.strip_prefix("::")?
            };
            Some((module, table))
        })
        .collect();
    match own.iter().find(|(m, _)| *m == module) {
        Some((_, table)) => Some(table),
        // `--model Post` resolved by guess: one project model of that name.
        None if own.len() == 1 => Some(own[0].1),
        None => None,
    }
}

/// Template emitted by `make:viewset <Name>` (default). Uses the
/// `#[derive(ViewSet)]` shape with a mount-time `PgPool` —
/// appropriate for single-tenant projects (api / fullstack
/// templates).
pub(super) fn viewset_template_pool(
    name: &str,
    model: &ModelRef,
    snake: &str,
    route: &str,
    crate_root: &str,
    app_crate: &str,
    codenames: bool,
) -> String {
    let ModelRef {
        path: model_path,
        name: model,
    } = model;
    // Codenames are checked only with `tenancy`; without it they refuse to mount.
    let guard = if codenames {
        "// Writes need a guard: add `permissions(create = \"...\", ...)` behind\n    \
         // an auth layer, then drop `read_only`."
    } else {
        "// Writes need a guard: put your own auth layer in front, then swap\n    \
         // `read_only` for `allow_anonymous` (codenames need `tenancy`)."
    };
    format!(
        r#"//! Auto-scaffolded by `manage make:viewset {name}`.

use {crate_root}::ViewSet;

use {model_path};

#[derive(ViewSet)]
#[viewset(
    model        = {model},
    // Required: the only fields the API renders and a body may write.
    fields       = "id, ",
    filter_fields = "",
    search_fields = "",
    page_size    = 20,
    {guard}
    read_only,
)]
pub struct {name};

// Mount in src/main.rs: `urls::api()` has no pool, `nest_with` passes
// the server's. Nest a prefix once; merge more viewsets in one closure.
//
//   .nest_with("/api", |pool| {app_crate}::{snake}::{name}::router("{route}", pool))
"#
    )
}

/// Template emitted by `make:viewset <Name> --tenant`. Uses the
/// runtime-built `ViewSet::for_model(...).tenant_router(...)` shape
/// (#80) so each request resolves the per-tenant connection via the
/// `Tenant` extractor instead of capturing a single pool at mount
/// time. Required for tenancy projects.
///
/// Since v0.30, `tenant_router` carries the full static-router builder
/// chain (filter / search / ordering / pagination / permissions) so
/// the scaffold demonstrates each knob, with `// uncomment to enable`
/// markers next to each one.
pub(super) fn viewset_template_tenant(
    name: &str,
    model: &ModelRef,
    snake: &str,
    route: &str,
    crate_root: &str,
) -> String {
    let ModelRef {
        path: model_path,
        name: model,
    } = model;
    format!(
        r#"//! Auto-scaffolded by `manage make:viewset {name} --tenant`.
//!
//! Tenant-aware viewset: each request resolves the connection via
//! `{crate_root}::extractors::Tenant`, so the same `router()` serves
//! every tenant under their own subdomain / schema / database.
//!
//! Since v0.30 (#80), `tenant_router` carries the full static-router
//! builder chain — filter_fields / search_fields / ordering /
//! page_size / permissions_for_model all work in tenant mode too.

use std::sync::Arc;

use axum::Router;
use {crate_root}::core::Model as _;
use {crate_root}::tenancy::{{ModelBackend, RouterAuthExt as _}};
use {crate_root}::viewset::ViewSet;

use {model_path};

pub fn router() -> Router<()> {{
    // The only fields the API renders and a body may write.
    ViewSet::for_model({model}::SCHEMA, &["id"])
        // .filter_fields(&["status", "owner_id"])    // ?status=draft&owner_id=42
        // .search_fields(&["name", "description"])   // ?search=foo (ILIKE)
        // .ordering(&[("created_at", true)])         // default ORDER BY
        // .ordering_fields(&["name", "created_at"])  // ?ordering=-name allowlist
        // .page_size(20)
        .permissions_for_model::<{model}>()          // CRUD codenames
        // .read_only()                               // GET only
        .tenant_router("/api{route}")
        // The permission check needs a tenant user. `ModelBackend` is HTTP
        // Basic only; a browser session from `/login` is not read here (401).
        // Token clients: add `ApiKeyBackend` or a `JwtBackend` to the list.
        .require_auth(vec![Arc::new(ModelBackend)])
}}

// Mount in your urls.rs:
//
//   .merge(crate::{snake}::router())
"#
    )
}

/// Detect whether the project the scaffolder is running inside enables
/// the `tenancy` feature on its `rustango` dep — used to default
/// `make:viewset` to `--tenant` when no explicit flag is given.
/// Reads `./Cargo.toml` from the current working directory; returns
/// `false` (single-tenant default) on any read/parse failure so the
/// scaffolder doesn't break in odd environments.
///
/// Heuristic: look for a `[dependencies.rustango]` table with
/// `features = [..., "tenancy", ...]` OR an inline-table dep
/// (`rustango = {{ version = "...", features = [..., "tenancy"] }}`).
/// Cargo.toml syntax is well-defined; this is a substring check
/// rather than a full TOML parse to keep the binary light, but it's
/// strict enough — a `# tenancy` comment elsewhere wouldn't trigger.
/// Decide whether `make:viewset` should emit the tenant template
/// and whether to echo a one-line auto-detect notice. Pure
/// function — no I/O, no `cargo` introspection — so tests can
/// exercise every branch without chdir tricks. The actual Cargo.toml
/// inspection happens once in [`project_uses_tenancy`] and gets
/// passed in here.
///
/// Returns `(tenant_aware, echo_auto_detect)`.
pub(super) fn resolve_viewset_tenant_mode(
    explicit_tenant: bool,
    explicit_no_tenant: bool,
    project_tenancy: bool,
) -> (bool, bool) {
    if explicit_no_tenant {
        return (false, false);
    }
    if explicit_tenant {
        return (true, false);
    }
    // Auto-detect path. Echo only when we actually picked tenant.
    (project_tenancy, project_tenancy)
}

pub(super) fn project_uses_tenancy() -> bool {
    let Ok(s) = std::fs::read_to_string("Cargo.toml") else {
        return false;
    };
    // Find the rustango dep block. Either a bare key
    // `rustango = "..."` (no features at all → not tenancy) or a
    // structured form (`rustango = { features = [...] }`) or a
    // dedicated table `[dependencies.rustango]`. We scan the whole
    // file for both shapes since the line carrying the features may
    // be on its own.
    let lower = s.to_ascii_lowercase();
    let has_inline = lower.contains("rustango")
        && lower
            .lines()
            .any(|line| line.contains("rustango") && line.contains("\"tenancy\""));
    let has_table_block =
        lower.contains("[dependencies.rustango]") && lower.contains("\"tenancy\"");
    has_inline || has_table_block
}

/// The project's own crate name, as a Rust path segment.
///
/// A file under `src/bin/` is its own crate: `crate::jobs::X` written
/// there names the *binary*, not the project, so a worker reaching into
/// the app must spell it `my_app::jobs::X`. Falls back to `your_app`
/// when there is no readable Cargo.toml — an obvious placeholder beats
/// a plausible-looking wrong path.
pub(super) fn project_crate_name() -> String {
    std::fs::read_to_string("Cargo.toml")
        .ok()
        .as_deref()
        .and_then(package_name_from_cargo_toml)
        .unwrap_or_else(|| "your_app".to_string())
}

/// Pure half of [`project_crate_name`]: the `name` key of `[package]`,
/// hyphens folded to underscores the way cargo derives a crate name.
pub(super) fn package_name_from_cargo_toml(src: &str) -> Option<String> {
    let mut in_package = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        // `name = "my-app"`, tolerating whitespace around the `=`.
        if let Some(value) = trimmed
            .strip_prefix("name")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('='))
        {
            let name = value.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.replace('-', "_"));
            }
        }
    }
    None
}

/// `manage make:api_routes <app> [--tenant]` — emit
/// `src/<app>/api_routes.rs`, the per-app composer that merges
/// every viewset's router into a single `Router<()>` (#82).
///
/// Mirrors the working pattern in tango/src/regions/api_routes.rs:
/// each per-model file under `viewsets/` exposes a router fn, and
/// `api_routes::api()` composes them with `.merge(...)`. The
/// scaffold is intentionally minimal — placeholder comments mark
/// where to add `.merge(...)` lines as new viewsets are written.
///
/// Refuses to overwrite an existing file. Errors clearly when the
/// `src/<app>/` directory is missing — tells the user to run
/// `manage startapp <app>` first.
pub(super) fn make_api_routes_cmd<W: Write>(
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut tenant_aware = false;
    let mut crate_root: Option<String> = None;
    let mut filtered: Vec<String> = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--tenant" || a == "--tenant-aware" {
            tenant_aware = true;
        } else if a == "--crate" {
            // Phase 2b of #145 — same shape as `parse_name_and_model`
            // gained earlier in this PR; default stays `"rustango"`.
            let v = iter
                .next()
                .cloned()
                .ok_or_else(|| missing_value("--crate"))?;
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Err(MigrateError::Validation(
                    "--crate <name> must not be empty".into(),
                ));
            }
            crate_root = Some(trimmed.to_owned());
        } else if a == "--help" || a == "-h" {
            writeln!(w, "make:api_routes <app> [--tenant] [--crate <name>]")?;
            writeln!(
                w,
                "  Scaffold src/<app>/api_routes.rs — the per-app router composer."
            )?;
            writeln!(
                w,
                "  Use --tenant for tenancy projects (no PgPool argument; each"
            )?;
            writeln!(
                w,
                "  viewset resolves its own per-request connection via the Tenant"
            )?;
            writeln!(w, "  extractor).")?;
            writeln!(
                w,
                "  --crate <name> overrides the emitted `use …::sql::Pool;`"
            )?;
            writeln!(w, "  crate root (default: `rustango`).")?;
            return Ok(());
        } else if a.starts_with('-') {
            return Err(MigrateError::Validation(format!(
                "make:api_routes: unrecognized flag `{a}`"
            )));
        } else {
            filtered.push(a.clone());
        }
    }
    let crate_root = crate_root.unwrap_or_else(|| "rustango".into());
    let app = filtered.first().ok_or_else(|| {
        MigrateError::Validation(
            "app name is required (e.g. `manage make:api_routes regions`)".into(),
        )
    })?;
    if filtered.len() > 1 {
        return Err(MigrateError::Validation(format!(
            "make:api_routes: expected one app name, got {} ({:?})",
            filtered.len(),
            filtered
        )));
    }
    if !is_valid_app_name(app) {
        return Err(MigrateError::Validation(format!(
            "make:api_routes: app name `{app}` must match `[a-z_][a-z0-9_]*`"
        )));
    }

    let app_dir = std::path::PathBuf::from("src").join(app);
    if !app_dir.exists() {
        return Err(MigrateError::Validation(format!(
            "make:api_routes: src/{app}/ does not exist. Run `manage startapp {app}` first."
        )));
    }

    let path = app_dir.join("api_routes.rs");
    refuse_overwrite(&path)?;

    let app_crate = project_crate_name();
    let body = if tenant_aware {
        api_routes_template_tenant(app)
    } else {
        let mounts = app_viewset_mounts(app, &app_crate, &registered_models());
        api_routes_template_pool(app, &crate_root, &mounts)
    };
    write_source(&path, &body)?;
    writeln!(w, "wrote {}", path.display())?;
    writeln!(w, "  add `pub mod api_routes;` to src/{app}/mod.rs,")?;
    if tenant_aware {
        writeln!(w, "  then in src/urls.rs, inside `api()`:")?;
        writeln!(w, "    .merge(crate::{app}::api_routes::api())")?;
    } else {
        // `urls::api()` has no pool; `nest_with` hands over the server's.
        writeln!(w, "  then in src/main.rs, after `.api(urls::api())`:")?;
        writeln!(
            w,
            "    .nest_with(\"/api\", |pool| {app_crate}::{app}::api_routes::api(pool))"
        )?;
    }
    Ok(())
}

pub(super) fn is_valid_app_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `--tenant` template — uses each viewset's `tenant_router(...)`,
/// no `PgPool` threaded through `api()` since the per-tenant
/// connection is resolved per-request via the `Tenant` extractor.
pub(super) fn api_routes_template_tenant(app: &str) -> String {
    format!(
        r#"//! Auto-scaffolded by `manage make:api_routes {app} --tenant`.
//!
//! API routing for the `{app}` app: composes tenant viewsets into a
//! single `Router<()>`.
//!
//! Adding a resource:
//!   1. Run `manage make:viewset <Name> --model <Model> --tenant`.
//!   2. Add one `.merge(...)` line below.

use axum::Router;

pub fn api() -> Router<()> {{
    Router::new()
        // .merge(crate::<snake>::router())
}}
"#
    )
}

/// `(model, route)` for each registered model of `app`, routed as
/// `make:viewset` routes it.
pub(super) fn app_viewset_mounts(
    app: &str,
    app_crate: &str,
    entries: &[(&str, &str, &str)],
) -> Vec<(String, String)> {
    let module = format!("{app_crate}::{app}::models");
    entries
        .iter()
        .filter(|(path, _, _)| *path == module)
        .map(|(_, name, _)| {
            let model = ModelRef {
                path: format!("crate::{app}::models::{name}"),
                name: (*name).to_owned(),
            };
            let (route, _) = viewset_route(&model, app_crate, entries.iter().copied());
            ((*name).to_owned(), route)
        })
        .collect()
}

/// Default template (no `--tenant`) — single-pool projects. The
/// `pool` argument is threaded into per-model `router(prefix, pool)`
/// calls; the macro-derived ViewSet captures it at mount time.
/// `mounts` gives one commented line per model, at its table's route.
pub(super) fn api_routes_template_pool(
    app: &str,
    crate_root: &str,
    mounts: &[(String, String)],
) -> String {
    let merges: String = if mounts.is_empty() {
        "        // .merge(crate::<snake>::<Name>::router(\"/<table>\", _pool.clone()))\n"
            .to_owned()
    } else {
        mounts
            .iter()
            .map(|(model, route)| {
                let snake = pascal_to_snake(&format!("{model}ViewSet"));
                format!(
                    "        // .merge(crate::{snake}::{model}ViewSet::router(\"{route}\", _pool.clone()))\n"
                )
            })
            .collect()
    };
    format!(
        r#"//! Auto-scaffolded by `manage make:api_routes {app}`.
//!
//! API routing for the `{app}` app. Composes per-model viewsets
//! into a single `Router<()>`. Each viewset captures the supplied
//! pool at mount time.
//!
//! The pool is `sql::Pool`, not a driver-typed one: it dispatches on
//! the `DATABASE_URL` scheme, so this file compiles and runs on all
//! three backends. A `PgPool` here would pin the whole app to
//! Postgres — `ViewSet::router_pool` takes exactly this type.
//!
//! Adding a resource:
//!   1. Run `manage make:viewset <Model>ViewSet --model <Model>`.
//!   2. Uncomment or add its `.merge(...)` line below; the path is the
//!      model's table, as `make:viewset` prints it.

use axum::Router;
use {crate_root}::sql::Pool;

pub fn api(pool: Pool) -> Router<()> {{
    let _pool = pool;
    Router::new()
        // `main.rs` nests this under `/api`.
{merges}}}
"#
    )
}

pub(super) fn make_serializer_cmd<W: Write>(
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let (name, model, crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let model = ModelRef::resolve(
        model.as_deref().unwrap_or("Post"),
        std::path::Path::new("src"),
    );
    write_generated(
        w,
        &format!("{snake}.rs"),
        serializer_template(&name, &model, &crate_root),
    )
}

pub(super) fn serializer_template(name: &str, model: &ModelRef, crate_root: &str) -> String {
    let ModelRef {
        path: model_path,
        name: model,
    } = model;
    format!(
        r#"//! Auto-scaffolded by `manage make:serializer {name}`.

use {crate_root}::sql::Auto;
use {crate_root}::Serializer;

use {model_path};

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = {model})]
pub struct {name} {{
    // A serializer field must match its model field's type **exactly**.
    // The scaffolders emit `pub id: Auto<i64>` for a primary key, so
    // `pub id: i64` here does not compile — it fails inside the derive
    // with `expected &i64, found &Auto<i64>`, which does not obviously
    // point back to this line.
    //
    // `read_only` keeps it out of the writable set while still
    // returning it, which is what you want: drop the field entirely and
    // the API answers a create with no identifier, so the client cannot
    // address what it just made.
    #[serializer(read_only)]
    pub id: Auto<i64>,
    // pub title: String,
    // #[serializer(source = "body")]   // publish under a different name
    // pub content: String,
}}
"#
    )
}

pub(super) fn make_form_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _, crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:form {name}`.

use {crate_root}::Form as DeriveForm;

#[derive(DeriveForm)]
pub struct {name} {{
    #[form(min_length = 1, max_length = 200)]
    pub title: String,
    pub body: Option<String>,
}}
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

/// Scaffold a real [`crate::jobs::Job`] (#1455).
///
/// This verb used to emit a struct holding a `PgPool` with an inherent
/// `run(self: Arc<Self>)` and a comment wiring it to
/// `scheduler::every(..)` — a *scheduler task*, which is a real thing
/// the framework has, but not the one the verb is named after. Nothing
/// it produced could be `dispatch`ed, `register`ed, retried, backed off
/// or dead-lettered, and the hardcoded `PgPool` meant it did not compile
/// in a `--features sqlite` project at all.
///
/// The scheduler shape now lives under `make:scheduled`, which is what
/// it always was.
pub(super) fn make_job_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _, crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:job {name}`.
//!
//! A background job: enqueued from a handler, executed later by a
//! worker. For work that runs on a timer instead, see
//! `manage make:scheduled`.

use {crate_root}::jobs::{{Job, JobError}};
use serde::{{Deserialize, Serialize}};

/// The payload. `run` receives **only this** — no pool, no tenant, no
/// request context: workers are spawned tasks and inherit no
/// task-local state. Carry everything the job needs in these fields.
#[derive(Serialize, Deserialize)]
pub struct {name} {{
    pub id: i64,
}}

#[async_trait::async_trait]        // add `async-trait` to your Cargo.toml
impl Job for {name} {{
    /// Globally unique: this is the routing key a worker matches a
    /// queued row against. Two job types must not share one.
    const NAME: &'static str = "{snake}";

    /// A ceiling on **total attempts**, not on retries — 5 is one run
    /// plus four retries. Backoff between them is 1s, 2s, 4s, 8s.
    const MAX_ATTEMPTS: u32 = 5;

    async fn run(&self) -> Result<(), JobError> {{
        // Err(JobError::Retryable(..)) to back off and try again;
        // Err(JobError::Fatal(..)) to dead-letter immediately.
        let _ = self.id;
        Ok(())
    }}
}}

// Wire up wherever you build the queue — the web process, a worker
// binary from `make:worker`, or both:
//
//   queue.register::<{name}>().await;   // BEFORE start(); every
//                                       // process that calls start()
//                                       // must register every type it
//                                       // might pick up, or the row is
//                                       // locked and abandoned
//   queue.start().await;
//
// and to enqueue one, from a handler:
//
//   queue.dispatch(&{name} {{ id: 42 }}).await?;
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

/// Scaffold a fixed-interval scheduler task (#1455).
///
/// This is the shape `make:job` used to emit, under the name it should
/// always have had — and routed through `sql::Pool` rather than the
/// hardcoded `PgPool`, which made the old template uncompilable in a
/// non-Postgres project.
pub(super) fn make_scheduled_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _, crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let app_crate = project_crate_name();
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:scheduled {name}`.
//!
//! A task that runs on a timer, not in response to a request. For work
//! enqueued by a handler and executed by a worker, see
//! `manage make:job`.

use std::sync::Arc;

use {crate_root}::sql::Pool;

pub struct {name} {{
    pub pool: Pool,
}}

impl {name} {{
    pub async fn run(self: Arc<Self>) {{
        // The scheduler isolates panics per task, but a task that
        // returns early on error simply skips that tick — log, do not
        // swallow silently.
        let _ = &self.pool;
    }}
}}

// Start it from a worker binary (`manage make:worker`), not main.rs:
// main.rs has no pool, and code before `Cli::run()` runs for every CLI
// verb. In the worker, after `let pool = ...;`:
//
//   let scheduler = {crate_root}::scheduler::Scheduler::new();
//   let task = Arc::new({app_crate}::{snake}::{name} {{ pool: pool.clone() }});
//   scheduler.every("{snake}", std::time::Duration::from_secs(60), move || {{
//       let task = Arc::clone(&task);
//       async move {{ task.run().await }}
//   }});
//   let _schedule = scheduler.start(); // ticks until dropped
//
// The first run happens after one full interval, not immediately.
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

/// Scaffold a standalone worker binary.
///
/// The shape is three lines long and easy to get wrong in a way that
/// only shows up in production: a worker that awaits
/// `tokio::signal::ctrl_c()` handles SIGINT but **not** SIGTERM, which
/// is what `docker stop`, Kubernetes and systemd actually send — so the
/// drain never runs, the container is `SIGKILL`ed after its grace period,
/// and in-flight jobs are lost with nothing logged. `shutdown_signal()`
/// takes both (#1409). Encoding that once is the whole point of this
/// verb.
pub(super) fn make_worker_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _, crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    // `src/bin/*.rs` is its own crate, so job types must be named
    // through the project's library — `crate::` here would resolve to
    // the worker binary itself and never compile.
    let body = worker_template(&name, &snake, &crate_root, &project_crate_name());
    write_generated_bin(w, &snake, body)
}

pub(super) fn worker_template(
    name: &str,
    snake: &str,
    crate_root: &str,
    app_crate: &str,
) -> String {
    format!(
        r#"//! Auto-scaffolded by `manage make:worker {name}`.
//!
//! A standalone worker process: it drains the job queue and serves no
//! HTTP. Run it alongside the web process, or as its own container.
//!
//! Put this at `src/bin/{snake}.rs` and run it with `cargo run --bin {snake}`.

use std::sync::Arc;
use std::time::Duration;

use {crate_root}::jobs::{{DatabaseJobQueue, JobQueue}};
use {crate_root}::sql::Pool;

#[{crate_root}::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {{
    // A worker never runs main.rs, so it loads `.env` itself.
    let _ = dotenvy::dotenv();
    // DATABASE_URL, or DB_USER + DB_NAME; the error names both.
    let url = {crate_root}::env::database_url_from_env().map_err(|e| {{
        format!("{{e}}. Set it in your shell, or copy '.env.example' to '.env'.")
    }})?;
    let pool = Pool::connect(&url).await?;

    // The queue is tri-dialect despite the `Database` name — the table
    // DDL and the row-pickup strategy are chosen from the pool's dialect.
    DatabaseJobQueue::ensure_table_pool(&pool).await?;
    // A worker killed mid-job (OOM, SIGKILL) never reaches its shutdown,
    // so its rows stay locked. This unlocks them at start and every minute.
    let queue = Arc::new(
        DatabaseJobQueue::with_workers_pool(pool.clone(), 4)
            .reclaim_stuck_after(Duration::from_secs(300)),
    );

    // Register EVERY job type this queue might see, not just the ones
    // this process dispatches. A worker that picks up a row whose name
    // is unregistered here logs and returns *without unlocking it* — the
    // row is then stranded until the reclaim sweep, and
    // it does not show up in `pending_count()`.
    //
    // Note the crate name: this file is its own binary crate, so `crate::`
    // would mean *this* worker. Job types live in the library, and
    // `manage make:job WelcomeEmail` writes `src/welcome_email.rs`.
    //
    //   queue.register::<{app_crate}::welcome_email::WelcomeEmail>().await;

    queue.start().await;
    tracing::info!("{snake}: draining jobs");

    // SIGINT *and* SIGTERM. `tokio::signal::ctrl_c()` alone is
    // SIGINT-only, so under `docker stop` the drain below never runs.
    {crate_root}::shutdown::shutdown_signal().await;

    tracing::info!("{snake}: signal received, draining in-flight jobs");
    queue.shutdown().await;
    Ok(())
}}
"#
    )
}

/// Like [`write_generated`], but for `src/bin/` — where cargo
/// auto-discovers the target, so the "add `mod ...`" advice
/// `write_generated` prints would be wrong.
pub(super) fn write_generated_bin<W: Write>(
    w: &mut W,
    bin_name: &str,
    contents: String,
) -> Result<(), MigrateError> {
    let path = std::path::PathBuf::from("src")
        .join("bin")
        .join(format!("{bin_name}.rs"));
    refuse_overwrite(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_source(&path, &contents)?;
    writeln!(w, "wrote {}", path.display())?;
    // No `[[bin]]` stanza needed: cargo picks up `src/bin/*.rs` on its own.
    writeln!(w, "  run it with `cargo run --bin {bin_name}`")?;
    // A second binary makes plain `cargo run` ambiguous, which breaks the
    // `cargo run -- migrate` workflow every generated project's README
    // documents. Cargo's error names the binaries but not the fix.
    writeln!(
        w,
        "  NOTE: a second binary makes plain `cargo run` ambiguous. Add\n  \
         `default-run = \"<your-app>\"` under [package] in Cargo.toml to keep\n  \
         `cargo run -- migrate` working."
    )?;
    Ok(())
}

pub(super) fn make_notification_cmd<W: Write>(
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let (name, _, crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:notification {name}`.
//!
//! User-facing notification, sent as an email. To reach several channels
//! (email, broadcast, a database row), implement
//! `{crate_root}::notifications::Notification` instead.

use {crate_root}::email::Email;

pub struct {name} {{
    pub user_email: String,
    pub subject: String,
}}

impl {name} {{
    pub fn build_email(&self) -> Email {{
        Email::new()
            .to(&self.user_email)
            .from("noreply@example.com")
            .subject(&self.subject)
            .body("Hello — this notification was generated by {name}.")
    }}
}}
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

pub(super) fn make_middleware_cmd<W: Write>(
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    // Middleware template has no `rustango::` references today — the
    // `--crate` flag is still parsed/validated so the args shape stays
    // uniform across the `make:*` family.
    let (name, _, _crate_root) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:middleware {name}`.

use axum::body::Body;
use axum::http::{{Request, Response}};
use axum::middleware::Next;

pub async fn {snake}(req: Request<Body>, next: Next) -> Response<Body> {{
    // TODO: pre-handler logic
    let response = next.run(req).await;
    // TODO: post-handler logic
    response
}}

// Apply with:
//   router.layer(axum::middleware::from_fn({snake}))
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

pub(super) fn make_test_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    // Identifier, not type name (#1214). `docs/manage.md` documents
    // `make:test post_smoke`, and that invocation used to be rejected — a test
    // file holds `#[test] fn`s, and `fn PostSmoke()` would warn under
    // `non_snake_case`. `PostSmoke` still works and still lands on
    // `tests/post_smoke.rs`, so the documented form and the old form agree.
    let (name, _, crate_root) = parse_name_and_model_as(args, NameShape::Identifier)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:test {name}`.
//!
//! Integration test. Run with `cargo test --test {snake}`.

use {crate_root}::test_client::TestClient;
use axum::Router;
use axum::routing::get;

fn app() -> Router {{
    // This test touches no database. As soon as it does — the moment
    // `app()` opens a pool from `DATABASE_URL` — uncomment this. A test
    // in `tests/` is its own crate and never runs `main`, so nothing has
    // loaded `.env` for it and the variable reads as unset (#1299).
    // let _ = dotenvy::dotenv();

    Router::new().route("/hello", get(|| async {{ "hi" }}))
}}

#[tokio::test]
async fn {snake}() {{
    let client = TestClient::new(app());
    let r = client.get("/hello").send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "hi");
}}
"#
    );
    // Tests live in tests/ not src/
    let path = std::path::PathBuf::from("tests").join(format!("{snake}.rs"));
    refuse_overwrite(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_source(&path, &body)?;
    writeln!(w, "wrote {}", path.display())?;
    Ok(())
}
