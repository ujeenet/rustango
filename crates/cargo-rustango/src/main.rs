//! `cargo rustango new <name>` — Django-style project scaffolder.
//!
//! Cargo invokes external subcommands by spawning a binary called
//! `cargo-rustango` and passing `rustango` as the first argv. We
//! strip that prefix and dispatch to a verb handler:
//!
//!   $ cargo rustango new myapp --template fullstack
//!   $ cargo rustango new api_demo --template api
//!   $ cargo rustango new shop --template tenant --backend sqlite --features csrf
//!
//! Run `new` with no arguments on a terminal for the wizard (`wizard.rs`),
//! which asks the same questions as numbered menus and then prints the
//! equivalent command line. It sets the same fields the flags do, so there is
//! one code path deciding what a project contains (#1345).
//!
//! Three axes, independent:
//!
//! * `--template` — which files get written.
//! * `--backend`  — which database the project defaults to, shaping
//!                  `Cargo.toml`'s `default`, `.env.example`, the compose
//!                  file, and the settings tiers together.
//! * `--features` — rustango opt-ins no template turns on.
//!
//! The three templates correspond to the three rustango shapes:
//!
//! * `api`        — bare ORM + axum, no admin. For JSON-only services.
//! * `fullstack`  — ORM + auto-admin (the default; matches the v0.7 README quickstart).
//! * `tenant`     — multi-tenancy enabled, operator console wired,
//!                  apex/subdomain host dispatch via `Cli::tenancy()`.
//!
//! Each template writes a self-contained Cargo project into `<cwd>/<name>/`:
//!
//!   <name>/
//!     Cargo.toml
//!     .env.example
//!     docker-compose.yml
//!     migrations/
//!     src/
//!       main.rs        ← Cli::new()[.tenancy()].api(...).run() — single binary
//!       models.rs
//!       views.rs
//!       urls.rs
//!
//! Once written, the user typically runs:
//!
//!   $ cd `<name>` && docker compose up -d
//!   $ cargo run -- migrate
//!   $ cargo run

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod templates;
mod wizard;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    // Cargo passes our own crate name as the first real argv when
    // called as `cargo rustango ...`. Strip it so we see `["new",
    // "myapp", ...]`. Allow direct invocation `cargo-rustango new ...`
    // too — useful in tests and for users who installed the binary.
    let args: Vec<String> = match raw.iter().position(|s| s == "rustango") {
        Some(i) if i + 1 < raw.len() => raw[i + 1..].to_vec(),
        _ => raw[1..].to_vec(),
    };

    match args.first().map(String::as_str) {
        Some("new") => match cmd_new(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Some("--help") | Some("-h") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("cargo-rustango {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("error: unknown subcommand `{other}` (run with --help)");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!("cargo-rustango — Django-style project scaffolder for rustango");
    println!();
    println!("USAGE:");
    println!("  cargo rustango new <name> [--template api|fullstack|tenant]");
    println!("                           [--rustango-path <dir>]");
    println!();
    println!("TEMPLATES:");
    println!("  api        bare ORM + axum, no admin (JSON-only services)");
    println!("  fullstack  ORM + auto-admin (default)");
    println!("  tenant     multi-tenancy + operator console + tenancy_manage CLI");
    println!();
    println!("OPTIONS:");
    println!("  -t, --template <name>    project template (default: fullstack)");
    println!("  -b, --backend <name>     postgres | sqlite | mysql (default: postgres)");
    println!("  -F, --features <list>    extra rustango features, comma-separated");
    println!("  -i, --interactive        pick from numbered menus instead");
    println!("      --rustango-path <d>  depend on a local rustango checkout");
    println!("                           instead of the published crate");
    println!();
    println!("INTERACTIVE:");
    println!("  `cargo rustango new` with no arguments opens a wizard that asks");
    println!("  for each of the above, then prints the equivalent command line.");
    println!();
    println!("BACKEND:");
    println!("  --backend picks what `cargo run` uses and shapes .env.example,");
    println!("  docker-compose.yml, and the settings tiers to match. The other");
    println!("  two stay one flag away:");
    println!("    cargo run --no-default-features --features sqlite");
    println!();
    println!("FEATURES:");
    let width = OPTIONAL_FEATURES
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0);
    for (name, about) in OPTIONAL_FEATURES {
        println!("  {name:<width$}  {about}");
    }
    println!();
    println!("EXAMPLES:");
    println!("  cargo rustango new myblog");
    println!("  cargo rustango new api_demo --template api");
    println!("  cargo rustango new api_demo --template=api");
    println!("  cargo rustango new shop --template tenant");
    println!("  cargo rustango new edge --backend sqlite");
    println!("  cargo rustango new saas --template tenant --features csrf,sso,cache-redis");
    println!("  cargo rustango new ex1 --rustango-path ../rustango/crates/rustango");
}

/// Database backend a generated project builds and runs with by default.
///
/// Only `default` in the project's own `[features]` changes — all three
/// forwards stay defined, so switching later is still one flag away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    Sqlite,
    Mysql,
}

impl Backend {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "postgres" | "postgresql" | "pg" => Ok(Self::Postgres),
            "sqlite" | "sqlite3" => Ok(Self::Sqlite),
            "mysql" | "mariadb" => Ok(Self::Mysql),
            other => Err(format!(
                "unknown backend `{other}` — must be `postgres`, `sqlite`, or `mysql`"
            )),
        }
    }

    /// The project's own feature name, which forwards to `rustango/<name>`.
    pub fn feature(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
            Self::Mysql => "mysql",
        }
    }

    /// Connection URL for cargo running on the host.
    pub fn host_url(self, name: &str) -> String {
        match self {
            Self::Postgres => format!("postgres://rustango:rustango@localhost:5432/{name}_dev"),
            Self::Mysql => format!("mysql://rustango:rustango@localhost:3306/{name}_dev"),
            Self::Sqlite => self.compose_url(name),
        }
    }

    /// Same, from inside the compose network — the service name is the host.
    /// SQLite is a file, so both forms are the bind-mounted path.
    pub fn compose_url(self, name: &str) -> String {
        match self {
            Self::Postgres => format!("postgres://rustango:rustango@postgres:5432/{name}_dev"),
            Self::Mysql => format!("mysql://rustango:rustango@mysql:3306/{name}_dev"),
            Self::Sqlite => format!("sqlite://./{name}_dev.db?mode=rwc"),
        }
    }

    /// Compose service name, or `None` when the backend needs no server.
    pub fn service(self) -> Option<&'static str> {
        match self {
            Self::Postgres => Some("postgres"),
            Self::Mysql => Some("mysql"),
            Self::Sqlite => None,
        }
    }
}

/// Opt-in rustango features `--features` accepts on top of a template's own.
///
/// The second field is the one-line description shown by `--help`. Kept
/// curated rather than "every feature in the manifest": these are the ones
/// no template reaches, which is the whole reason the flag exists.
/// `tests/templates_name_real_features.rs` asserts the list stays both real
/// and complete.
pub const OPTIONAL_FEATURES: &[(&str, &str)] = &[
    (
        "tenancy",
        "multi-tenancy: tenant registry, per-tenant databases, operator console",
    ),
    ("csrf", "CSRF protection middleware for form POSTs"),
    ("sso", "OIDC single sign-on for application users"),
    ("admin-sso", "OIDC single sign-on for the admin site"),
    ("passkey", "WebAuthn / passkey authentication"),
    ("cache-redis", "Redis cache backend"),
    ("cache-page", "whole-page response caching"),
    ("email-smtp", "SMTP transport for the email framework"),
    ("mcp", "Model Context Protocol server for AI agents"),
    ("testkit", "test-only schema builders and model factories"),
    ("test_utils", "test-only constructors for downstream crates"),
];

#[derive(Debug, Clone, Copy)]
enum Template {
    Api,
    Fullstack,
    Tenant,
}

impl Template {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "api" => Ok(Self::Api),
            "fullstack" => Ok(Self::Fullstack),
            "tenant" => Ok(Self::Tenant),
            other => Err(format!(
                "unknown template `{other}` — must be `api`, `fullstack`, or `tenant`"
            )),
        }
    }

    /// The `--template` spelling, for echoing a choice back as a command.
    pub fn name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Fullstack => "fullstack",
            Self::Tenant => "tenant",
        }
    }

    /// rustango features this template needs, before `--features` extras.
    ///
    /// The backend is deliberately absent (#1211) — see [`Template::rustango_dep`].
    pub fn base_features(self) -> &'static [&'static str] {
        match self {
            // Bare ORM + axum + manage dispatcher; no auto-admin UI.
            Self::Api => &["manage"],
            // `batteries` is rustango's default set minus the backend.
            Self::Fullstack => &["batteries"],
            Self::Tenant => &["batteries", "tenancy"],
        }
    }

    /// The full `rustango = …` dependency value for a generated `Cargo.toml`.
    ///
    /// `path` is `--rustango-path`: it swaps the crates.io source for a local
    /// checkout while keeping the same feature selection, so an in-repo example
    /// needs no hand edit and the generate-and-check test can validate the
    /// working tree rather than the last release (#1211).
    ///
    /// RELEASE ORDER IS LOAD-BEARING (#1217). A version-pinned project resolves
    /// against the *published* rustango, while this list is written against the
    /// tree in this repo; naming a feature the release lacks fails at
    /// resolution, before anything compiles. Two things prevent that: all four
    /// crates publish from one version with `rustango` first, and
    /// `tests/templates_name_real_features.rs` asserts every name here is
    /// defined in `crates/rustango/Cargo.toml`.
    ///
    /// The backend is NOT named here (#1211). The generated `[features]` block
    /// forwards it, because `#[derive(Model)]` gates its emissions on the
    /// *project's* features — a cfg inside a derive resolves against the
    /// destination crate. Pinning `rustango/postgres` while the project's own
    /// `postgres` feature was off made the two disagree.
    fn rustango_dep(self, path: Option<&str>, extras: &[String]) -> String {
        let feats = feature_list(self.base_features(), extras);
        match path {
            Some(p) => {
                format!(r#"{{ path = "{p}", default-features = false, features = [{feats}] }}"#)
            }
            // Track our own version, bumped in lockstep with rustango, so a
            // published scaffolder always pins a real, current release. It was
            // hardcoded pre-v0.29 and rotted to `"0.23"` (#79).
            None => format!(
                r#"{{ version = "{}", default-features = false, features = [{feats}] }}"#,
                mm_version()
            ),
        }
    }
}

/// `"a", "b"` — a template's own features plus `--features` extras, deduped,
/// order preserved so the emitted manifest is stable.
fn feature_list(base: &[&str], extras: &[String]) -> String {
    let mut all: Vec<&str> = base.to_vec();
    for e in extras {
        if !all.contains(&e.as_str()) {
            all.push(e);
        }
    }
    all.iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reject typos and misdirected backends before a project is written.
///
/// A bad feature name is otherwise invisible until `cargo build` fails at
/// dependency resolution, with an error about the framework rather than the
/// flag that caused it.
fn validate_features(extras: &[String], template: Template) -> Result<(), String> {
    for f in extras {
        if ["postgres", "sqlite", "mysql"].contains(&f.as_str()) {
            return Err(format!(
                "`{f}` is a database backend — pass `--backend {f}` instead. \
                 The project forwards its backend through its own feature, so \
                 naming it here would break `#[derive(Model)]` (#1211)."
            ));
        }
        if template.base_features().contains(&f.as_str()) {
            continue;
        }
        if !OPTIONAL_FEATURES.iter().any(|(n, _)| n == f) {
            let known: Vec<&str> = OPTIONAL_FEATURES.iter().map(|(n, _)| *n).collect();
            return Err(format!(
                "unknown feature `{f}` — available: {}.\n\
                 Anything else can be added to the generated Cargo.toml by hand.",
                known.join(", ")
            ));
        }
    }
    Ok(())
}

/// Major.minor of the current `cargo-rustango` build — e.g.
/// `"0.28.4"` → `"0.28"`. Cargo's caret semantics pin the same
/// way (`"0.28"` = `"^0.28.0"`), so newly scaffolded projects
/// resolve to whatever 0.28.x is current on crates.io.
fn mm_version() -> String {
    let full = env!("CARGO_PKG_VERSION");
    full.rsplit_once('.')
        .map(|(mm, _patch)| mm.to_owned())
        .unwrap_or_else(|| full.to_owned())
}

#[derive(Debug)]
struct NewArgs {
    name: String,
    template: Template,
    /// `--backend <name>`: which backend the project defaults to. All three
    /// stay switchable; this picks the one `cargo run` uses and shapes the
    /// generated `.env`, compose file, and settings tiers to match.
    backend: Backend,
    /// `--features a,b`: rustango opt-ins on top of the template's own.
    features: Vec<String>,
    /// `--rustango-path <dir>`: emit a path dependency instead of the crates.io
    /// version. For in-repo examples and for testing the working tree (#1211).
    rustango_path: Option<String>,
    /// `-i` / `--interactive`, or a bare `new` on a terminal: ask the rest.
    interactive: bool,
}

fn cmd_new(args: &[String]) -> Result<(), String> {
    // A bare `new` on a terminal is a request for the wizard; piped or in CI
    // it stays the old "missing project name" error, so scripts don't hang on
    // a prompt nobody can answer.
    let asked = args.iter().any(|a| a == "-i" || a == "--interactive");
    let bare = args.is_empty() && std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut parsed = parse_new_args(args, asked || bare)?;
    if parsed.interactive {
        let named = !parsed.name.is_empty();
        parsed = wizard::run(parsed, named)?;
    }
    validate_name(&parsed.name)?;

    let root = PathBuf::from(&parsed.name);
    if root.exists() {
        return Err(format!(
            "destination directory `{}` already exists — pick a fresh name or remove it first",
            root.display()
        ));
    }

    println!(
        "scaffolding `{}` (template: {}, backend: {}) at {}",
        parsed.name,
        parsed.template.name(),
        parsed.backend.feature(),
        root.display()
    );

    fs::create_dir_all(&root).map_err(|e| format!("create_dir_all({}): {e}", root.display()))?;
    write_project(&root, &parsed)?;

    println!();
    println!("done. next:");
    println!("  cd {}", parsed.name);
    println!("  cp .env.example .env");
    // SQLite is a file the first migrate creates — nothing to boot.
    if parsed.backend.service().is_some() {
        println!("  docker compose up -d");
    }
    println!(
        "  cargo run -- makemigrations  # generate schema migrations (incl. system/ framework tables)"
    );
    println!("  cargo run -- migrate         # apply them");
    println!("  cargo run                    # boot the HTTP server");
    println!("  cargo run -- --help          # full verb list");
    Ok(())
}

/// Parse `new`'s arguments.
///
/// `interactive` relaxes the one requirement the flags impose — a project
/// name — because the wizard asks for it. Everything else already has a
/// default, so a bare `cargo rustango new` is a complete request.
fn parse_new_args(args: &[String], interactive: bool) -> Result<NewArgs, String> {
    let mut name: Option<String> = None;
    let mut template = Template::Fullstack;
    let mut backend = Backend::Postgres;
    let mut features: Vec<String> = Vec::new();
    let mut rustango_path: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--backend" | "-b" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--backend requires a value".to_owned())?;
                backend = Backend::parse(v)?;
            }
            _ if arg.starts_with("--backend=") => {
                backend = Backend::parse(&arg["--backend=".len()..])?;
            }
            _ if arg.starts_with("-b=") => {
                backend = Backend::parse(&arg["-b=".len()..])?;
            }
            // Repeatable and comma-separated both work, matching cargo's own
            // `--features` so muscle memory carries over.
            "--features" | "-F" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--features requires a value".to_owned())?;
                features.extend(split_features(v));
            }
            _ if arg.starts_with("--features=") => {
                features.extend(split_features(&arg["--features=".len()..]));
            }
            _ if arg.starts_with("-F=") => {
                features.extend(split_features(&arg["-F=".len()..]));
            }
            "--template" | "-t" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--template requires a value".to_owned())?;
                template = Template::parse(v)?;
            }
            // #1211 — the equals form is universal CLI convention (cargo's own
            // flags take it) and used to be rejected as an unknown flag.
            _ if arg.starts_with("--template=") => {
                template = Template::parse(&arg["--template=".len()..])?;
            }
            _ if arg.starts_with("-t=") => {
                template = Template::parse(&arg["-t=".len()..])?;
            }
            // #1211 — generate against a local checkout instead of crates.io.
            // Every in-repo example has to hand-rewrite the dependency line
            // otherwise, and without this the "does a generated project
            // compile?" test can only ever validate the last *release*, not
            // the working tree — which is how a broken template shipped.
            "--rustango-path" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--rustango-path requires a value".to_owned())?;
                rustango_path = Some(v.clone());
            }
            _ if arg.starts_with("--rustango-path=") => {
                rustango_path = Some(arg["--rustango-path=".len()..].to_owned());
            }
            // Consumed by the caller, which decides before parsing whether a
            // terminal is available; accepted here so it isn't "unknown".
            "--interactive" | "-i" => {}
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}` (run --help)"));
            }
            other => {
                if name.is_some() {
                    return Err(format!("unexpected positional argument `{other}`"));
                }
                name = Some(other.to_owned());
            }
        }
    }
    let name = match name {
        Some(n) => n,
        None if interactive => String::new(),
        None => {
            return Err(
                "missing project name (e.g. `cargo rustango new myapp`, or `new` alone \
                 for the interactive wizard)"
                    .to_owned(),
            )
        }
    };
    // Repeated flags can name the same feature twice; keep first mention.
    let mut seen = Vec::new();
    features.retain(|f| {
        let fresh = !seen.contains(f);
        seen.push(f.clone());
        fresh
    });
    validate_features(&features, template)?;
    Ok(NewArgs {
        name,
        template,
        backend,
        features,
        rustango_path,
        interactive,
    })
}

/// `"a, b,,c"` → `["a", "b", "c"]`. Cargo accepts commas or spaces in one
/// `--features` value; so do we.
fn split_features(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Names cargo refuses as a **binary target**, which every template emits.
///
/// These pass the identifier check but produce a project whose manifest
/// cargo will not even load — "the binary target name `build` is forbidden,
/// it conflicts with cargo's build directory names". Better to refuse before
/// writing fifteen files the user then has to delete (#1358).
const RESERVED_TARGET_NAMES: &[&str] = &["build", "deps", "examples", "incremental"];

fn validate_name(name: &str) -> Result<(), String> {
    if RESERVED_TARGET_NAMES.contains(&name) {
        return Err(format!(
            "`{name}` cannot be a project name — cargo forbids it as a binary target \
             because it conflicts with its own build directory names"
        ));
    }
    let valid = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        return Err(format!(
            "`{name}` is not a valid Cargo crate name — use [A-Za-z_][A-Za-z0-9_-]*"
        ));
    }
    Ok(())
}

fn write_project(root: &Path, args: &NewArgs) -> Result<(), String> {
    let name = &args.name;
    let template = args.template;
    let backend = args.backend;

    write(
        root,
        "Cargo.toml",
        &templates::cargo_toml(
            name,
            template,
            backend,
            &args.features,
            args.rustango_path.as_deref(),
        ),
    )?;
    write(root, ".env.example", &templates::env_example(name, backend))?;
    write(root, ".gitignore", templates::GITIGNORE)?;
    write(root, "rust-toolchain.toml", templates::RUST_TOOLCHAIN)?;
    write(
        root,
        "docker-compose.yml",
        &templates::docker_compose(name, backend),
    )?;
    write(root, "Dockerfile", templates::dockerfile())?;
    write(
        root,
        "README.md",
        &templates::readme(name, template, backend),
    )?;

    // Tiered settings (#87) — config/default.toml for shared knobs +
    // one <env>_settings.toml per tier. Runtime picks the tier from
    // RUSTANGO_ENV (default `dev`), so a fresh `cargo run` works
    // without any TOML edits.
    write(
        root,
        "config/default.toml",
        &templates::config_default_toml(name, backend),
    )?;
    write(
        root,
        "config/dev_settings.toml",
        &templates::config_dev_settings_toml(name, backend),
    )?;
    write(
        root,
        "config/staging_settings.toml",
        &templates::config_staging_settings_toml(name, backend),
    )?;
    write(
        root,
        "config/prod_settings.toml",
        &templates::config_prod_settings_toml(name),
    )?;

    fs::create_dir_all(root.join("migrations")).map_err(|e| format!("create migrations/: {e}"))?;

    write(root, "src/main.rs", templates::main_rs(template))?;
    write(root, "src/models.rs", &templates::models_rs(template))?;
    write(root, "src/views.rs", templates::VIEWS_RS)?;
    write(root, "src/urls.rs", &templates::urls_rs(template))?;

    // Tenant projects get an empty `system/migrations/` folder — the
    // framework's own tables (`rustango_orgs`, `rustango_users`, roles/
    // permissions, …) are NOT shipped as hardcoded bootstrap JSON.
    // Instead the first `cargo run -- makemigrations` generates them
    // into `system/migrations/` from the compiled models (reflecting the
    // enabled feature flags), and `cargo run -- migrate` applies them —
    // the normal Django flow. `.gitkeep` keeps the dir under version
    // control until the migrations land.
    if matches!(template, Template::Tenant) {
        write(root, "system/migrations/.gitkeep", "")?;
    }

    Ok(())
}

fn write(root: &Path, rel: &str, body: &str) -> Result<(), String> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create_dir_all({}): {e}", parent.display()))?;
    }
    fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("  + {rel}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_version_strips_patch() {
        let v = mm_version();
        // Must look like "MAJOR.MINOR" — no trailing ".PATCH" component.
        assert!(
            v.matches('.').count() == 1 || (v.matches('.').count() == 0 && !v.is_empty()),
            "expected `major.minor` shape, got `{v}`"
        );
        // Sanity: matches the leading dotted prefix of CARGO_PKG_VERSION.
        let full = env!("CARGO_PKG_VERSION");
        assert!(
            full.starts_with(&v),
            "expected `{full}` to start with `{v}`"
        );
    }

    /// Regression guard for #79: every scaffold template must pin
    /// rustango to the same major.minor as the scaffolder build,
    /// not a hardcoded literal that rots silently as the framework
    /// version moves forward.
    #[test]
    fn every_template_pins_current_version() {
        let mm = mm_version();
        let needle = format!("\"{mm}\"");
        for template in [Template::Api, Template::Fullstack, Template::Tenant] {
            let dep = template.rustango_dep(None, &[]);
            assert!(
                dep.contains(&needle),
                "template {template:?} dep `{dep}` does not pin v{mm}"
            );
        }
    }

    /// Tiered settings (#87 slice 3) — every fresh project must
    /// ship `default.toml` + the three `<env>_settings.toml` tiers.
    /// The runtime auto-selects via `RUSTANGO_ENV`, defaulting to
    /// `dev`, so `cargo run` Just Works without explicit env vars.
    #[test]
    fn config_templates_emit_default_plus_three_tiers() {
        // Test the rendered bodies, not the on-disk write — keeps
        // this a pure-string regression that doesn't need a tempdir.
        for name in ["acme", "demo_app"] {
            let default_body = templates::config_default_toml(name, Backend::Postgres);
            assert!(
                default_body.contains(name),
                "config_default_toml({name}) must mention the project name; got: {default_body}"
            );
            let dev = templates::config_dev_settings_toml(name, Backend::Postgres);
            assert!(
                dev.contains("(dev)") || dev.contains("dev_settings"),
                "dev tier should be visually distinguishable; got: {dev}"
            );
            let staging = templates::config_staging_settings_toml(name, Backend::Postgres);
            assert!(
                staging.contains("staging") && staging.contains("retention_days"),
                "staging tier missing retention_days; got: {staging}"
            );
            let prod = templates::config_prod_settings_toml(name);
            assert!(
                prod.contains("strict") && prod.contains("hsts_max_age_secs"),
                "prod tier should default to strict security headers; got: {prod}"
            );
        }
    }

    /// Regression guard against the original #79 footgun — no
    /// scaffold template may emit a yanked version literal.
    #[test]
    fn no_template_pins_yanked_version() {
        // Versions known to be yanked on crates.io (rustango-macros
        // ^0.23.0 was yanked, breaking `rustango = "0.23"` resolution).
        const YANKED: &[&str] = &["0.23"];
        for template in [Template::Api, Template::Fullstack, Template::Tenant] {
            let dep = template.rustango_dep(None, &[]);
            for ver in YANKED {
                let needle = format!("\"{ver}\"");
                assert!(
                    !dep.contains(&needle),
                    "template {template:?} pins yanked rustango v{ver}: `{dep}`"
                );
            }
        }
    }

    // ---- #86 — Dockerfile + cargo-watch rust service in scaffolder ----

    /// `Dockerfile` template emits a working rust toolchain image
    /// with `cargo-watch` preinstalled — the foundation of the
    /// hot-reload dev loop the docker-compose.yml expects.
    #[test]
    fn dockerfile_emits_rust_toolchain_with_cargo_watch() {
        let body = templates::dockerfile();
        assert!(
            body.contains("FROM rust:"),
            "Dockerfile must base on a rust image, got `{body}`"
        );
        assert!(
            body.contains("cargo install cargo-watch"),
            "Dockerfile must preinstall cargo-watch (powers the docker-compose.yml \
             hot-reload command), got `{body}`"
        );
        assert!(
            body.contains("WORKDIR /app"),
            "Dockerfile must set WORKDIR /app to match docker-compose.yml's bind \
             mount target, got `{body}`"
        );
    }

    /// `docker-compose.yml` ships both postgres AND a rust service
    /// running `cargo watch -x run`, plus the three named cargo
    /// volumes that preserve incremental build state.
    #[test]
    fn docker_compose_bundles_rust_service_with_cargo_watch() {
        let body = templates::docker_compose("myapp", Backend::Postgres);
        // Postgres half (regression guard — don't lose the original
        // service when adding the rust one).
        assert!(body.contains("image: postgres:"), "{body}");
        assert!(body.contains("POSTGRES_DB: myapp_dev"), "{body}");
        // Rust half (#86 additions).
        assert!(body.contains("rust:"), "rust service block missing: {body}");
        assert!(
            body.contains("cargo watch -x run"),
            "rust service must run cargo-watch, got: {body}"
        );
        assert!(
            body.contains("build: ."),
            "rust service must build from the project Dockerfile, got: {body}"
        );
        // Cargo cache volumes — without these, every `up` triggers
        // a full from-scratch rebuild (the worst dev UX possible).
        for vol in ["cargo-target", "cargo-registry", "cargo-git"] {
            assert!(
                body.contains(vol),
                "expected cargo cache volume `{vol}` in compose, got: {body}"
            );
        }
        // depends_on healthy postgres so rust doesn't start before DB.
        assert!(
            body.contains("depends_on:"),
            "rust service must depend on postgres being healthy, got: {body}"
        );
    }

    /// Every scaffolded `src/main.rs` mounts `.with_welcome()` so a
    /// fresh `cargo run` boots to the friendly "rustango — it works!"
    /// page rather than a 404. Recommended for first-run UX since
    /// v0.29.12 — drop the call manually once a real `/` handler is
    /// wired.
    #[test]
    fn every_main_template_mounts_with_welcome() {
        for template in [Template::Api, Template::Fullstack, Template::Tenant] {
            let body = templates::main_rs(template);
            assert!(
                body.contains(".with_welcome()"),
                "template {template:?} src/main.rs should chain `.with_welcome()` \
                 — the scaffolded entrypoint, got:\n{body}"
            );
        }
    }

    // ---- #1345 — `--backend` and `--features` ----

    fn parse(argv: &[&str]) -> Result<NewArgs, String> {
        let owned: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        parse_new_args(&owned, false)
    }

    /// `--backend` moves `default`, and only `default` — all three forwards
    /// stay so the project can still be built the other two ways.
    #[test]
    fn backend_picks_the_default_feature_only() {
        for (flag, feature) in [
            ("postgres", "postgres"),
            ("sqlite", "sqlite"),
            ("mysql", "mysql"),
            ("pg", "postgres"),
            ("mariadb", "mysql"),
        ] {
            let args = parse(&["app", "--backend", flag]).expect(flag);
            let toml = templates::cargo_toml("app", args.template, args.backend, &[], None);
            assert!(
                toml.contains(&format!(r#"default = ["{feature}"]"#)),
                "--backend {flag} should default to {feature}:\n{toml}"
            );
            for other in ["postgres", "sqlite", "mysql"] {
                assert!(
                    toml.contains(&format!(r#"{other} = ["rustango/{other}"]"#)),
                    "the `{other}` forward must survive --backend {flag}:\n{toml}"
                );
            }
        }
    }

    #[test]
    fn backend_accepts_both_flag_forms() {
        for argv in [
            vec!["app", "--backend", "sqlite"],
            vec!["app", "--backend=sqlite"],
            vec!["app", "-b", "sqlite"],
            vec!["app", "-b=sqlite"],
        ] {
            let args = parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(args.backend, Backend::Sqlite, "{argv:?}");
        }
        assert!(parse(&["app", "--backend", "oracle"]).is_err());
    }

    /// Comma-separated, space-separated, and repeated all mean the same thing
    /// — and a feature named twice lands once.
    #[test]
    fn features_accumulate_without_duplicates() {
        for argv in [
            vec!["app", "--features", "csrf,sso"],
            vec!["app", "--features=csrf,sso"],
            vec!["app", "--features", "csrf sso"],
            vec!["app", "-F", "csrf", "-F", "sso"],
            vec!["app", "--features", "csrf,sso,csrf"],
        ] {
            let args = parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(args.features, vec!["csrf", "sso"], "{argv:?}");
        }
    }

    /// Extras join the template's own list rather than replacing it.
    #[test]
    fn features_extend_the_template_list() {
        let args = parse(&["app", "--template", "tenant", "--features", "csrf"]).expect("parse");
        let dep = args.template.rustango_dep(None, &args.features);
        assert!(
            dep.contains(r#"features = ["batteries", "tenancy", "csrf"]"#),
            "extras must append to the template's own: {dep}"
        );
    }

    /// A backend named in `--features` would pin `rustango/<backend>` while
    /// the project's own feature stayed off — exactly the mismatch #1211 fixed.
    #[test]
    fn features_rejects_backends_and_points_at_the_flag() {
        for backend in ["postgres", "sqlite", "mysql"] {
            let err = parse(&["app", "--features", backend]).expect_err(backend);
            assert!(
                err.contains(&format!("--backend {backend}")),
                "should redirect to --backend: {err}"
            );
        }
    }

    /// Caught at parse time, where the message can name the flag — not at
    /// dependency resolution, where it names the framework.
    #[test]
    fn features_rejects_unknown_names() {
        let err = parse(&["app", "--features", "cache-redis,nosuchthing"]).expect_err("unknown");
        assert!(err.contains("nosuchthing"), "{err}");
        assert!(
            err.contains("cache-redis"),
            "should list what is valid: {err}"
        );
        // A feature the template already turns on is a harmless no-op.
        assert!(parse(&["app", "--features", "batteries"]).is_ok());
    }

    /// Four names pass the identifier check and then produce a manifest
    /// cargo refuses to load — "the binary target name `build` is
    /// forbidden" (#1358). Refused before anything is written.
    #[test]
    fn names_cargo_forbids_as_a_binary_target_are_refused() {
        for n in ["build", "deps", "examples", "incremental"] {
            let err = validate_name(n).expect_err(n);
            assert!(err.contains(n), "should name it: {err}");
            assert!(
                err.contains("binary target"),
                "should say why cargo objects: {err}"
            );
        }
        // Not reserved — these load fine, so they stay accepted.
        for n in ["myblog", "test", "std", "core", "crate_thing"] {
            validate_name(n).unwrap_or_else(|e| panic!("{n} should be allowed: {e}"));
        }
    }

    /// `.env.example` must not ship a `RUSTANGO_SESSION_SECRET` value: one
    /// that is not 32 bytes of base64 is discarded silently, so the project
    /// looks configured and is not (#1359).
    #[test]
    fn the_env_template_ships_no_live_session_secret() {
        let env = templates::env_example("app", Backend::Postgres);
        for line in env.lines() {
            let t = line.trim();
            assert!(
                !(t.starts_with("RUSTANGO_SESSION_SECRET=")
                    && t.len() > "RUSTANGO_SESSION_SECRET=".len()),
                "an uncommented secret with a value would be silently discarded: {t}"
            );
        }
        assert!(
            env.contains("openssl rand -base64 32"),
            "it should still say how to make a real one:\n{env}"
        );
    }

    /// The flags shape more than `Cargo.toml`: a project generated for one
    /// backend must not tell the user to connect to another.
    #[test]
    fn backend_reaches_env_compose_and_settings() {
        let name = "app";
        for (backend, needle) in [
            (Backend::Postgres, "postgres://"),
            (Backend::Mysql, "mysql://"),
            (Backend::Sqlite, "sqlite://"),
        ] {
            let env = templates::env_example(name, backend);
            assert!(env.contains(needle), "{backend:?} .env.example: {env}");
            let dev = templates::config_dev_settings_toml(name, backend);
            assert!(dev.contains(needle), "{backend:?} dev tier: {dev}");
        }
        // SQLite is a file — compose must not invent a server for it.
        let lite = templates::docker_compose(name, Backend::Sqlite);
        assert!(!lite.contains("image: "), "no db image for sqlite:\n{lite}");
        assert!(
            !lite.contains("depends_on:"),
            "nothing to depend on:\n{lite}"
        );
        assert!(lite.contains("rust:"), "the dev container stays:\n{lite}");

        let my = templates::docker_compose(name, Backend::Mysql);
        assert!(my.contains("image: mysql:8"), "{my}");
        assert!(my.contains("MYSQL_DATABASE: app_dev"), "{my}");
        assert!(my.contains("depends_on:"), "{my}");
    }

    /// `.env.example` defaults must work out-of-box for `docker
    /// compose up -d` (host = `postgres`, bind = `0.0.0.0`). Users
    /// running cargo on the host edit `postgres` -> `localhost`.
    #[test]
    fn env_example_defaults_to_docker_friendly_values() {
        let body = templates::env_example("myapp", Backend::Postgres);
        assert!(
            body.contains("@postgres:5432/"),
            "DATABASE_URL host must default to `postgres` (compose service name), \
             got: {body}"
        );
        assert!(
            body.contains("RUSTANGO_BIND=0.0.0.0:8080"),
            "bind must default to 0.0.0.0 so the container's exposed port is \
             reachable from the host, got: {body}"
        );
    }
}
