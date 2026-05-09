//! `cargo rustango new <name>` — Django-style project scaffolder.
//!
//! Cargo invokes external subcommands by spawning a binary called
//! `cargo-rustango` and passing `rustango` as the first argv. We
//! strip that prefix and dispatch to a verb handler:
//!
//!   $ cargo rustango new myapp --template fullstack
//!   $ cargo rustango new api_demo --template api
//!   $ cargo rustango new shop --template tenant
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
//!   $ cd <name> && docker compose up -d
//!   $ cargo run -- migrate
//!   $ cargo run

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod templates;

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
    println!();
    println!("TEMPLATES:");
    println!("  api        bare ORM + axum, no admin (JSON-only services)");
    println!("  fullstack  ORM + auto-admin (default)");
    println!("  tenant     multi-tenancy + operator console + tenancy_manage CLI");
    println!();
    println!("EXAMPLES:");
    println!("  cargo rustango new myblog");
    println!("  cargo rustango new api_demo --template api");
    println!("  cargo rustango new shop --template tenant");
}

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

    fn rustango_features(self) -> String {
        // Track our own (cargo-rustango) version, which is bumped
        // in lockstep with the rustango crate via the workspace
        // `version = "..."` declaration. Pin scaffolded projects
        // to the same major.minor so a published scaffolder always
        // produces a project that resolves against a real, current
        // rustango release. Pre-v0.29 the version was hardcoded —
        // and consequently rotted to `"0.23"` while the framework
        // moved to v0.28+, breaking `cargo build` on every fresh
        // tenant project (#79).
        let v = mm_version();
        match self {
            // Bare ORM + axum + manage dispatcher; no auto-admin UI.
            Self::Api => format!(
                r#"{{ version = "{v}", default-features = false, features = ["postgres", "manage"] }}"#
            ),
            Self::Fullstack => format!(r#""{v}""#),
            Self::Tenant => format!(r#"{{ version = "{v}", features = ["tenancy"] }}"#),
        }
    }
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

struct NewArgs {
    name: String,
    template: Template,
}

fn cmd_new(args: &[String]) -> Result<(), String> {
    let parsed = parse_new_args(args)?;
    validate_name(&parsed.name)?;

    let root = PathBuf::from(&parsed.name);
    if root.exists() {
        return Err(format!(
            "destination directory `{}` already exists — pick a fresh name or remove it first",
            root.display()
        ));
    }

    println!(
        "scaffolding `{}` (template: {:?}) at {}",
        parsed.name,
        parsed.template,
        root.display()
    );

    fs::create_dir_all(&root).map_err(|e| format!("create_dir_all({}): {e}", root.display()))?;
    write_project(&root, &parsed)?;

    println!();
    println!("done. next:");
    println!("  cd {}", parsed.name);
    println!("  cp .env.example .env");
    println!("  docker compose up -d");
    println!("  cargo run -- migrate    # apply pending migrations");
    println!("  cargo run               # boot the HTTP server");
    println!("  cargo run -- --help     # full verb list");
    Ok(())
}

fn parse_new_args(args: &[String]) -> Result<NewArgs, String> {
    let mut name: Option<String> = None;
    let mut template = Template::Fullstack;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--template" | "-t" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--template requires a value".to_owned())?;
                template = Template::parse(v)?;
            }
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
    let name =
        name.ok_or_else(|| "missing project name (e.g. `cargo rustango new myapp`)".to_owned())?;
    Ok(NewArgs { name, template })
}

fn validate_name(name: &str) -> Result<(), String> {
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

    write(root, "Cargo.toml", &templates::cargo_toml(name, template))?;
    write(root, ".env.example", &templates::env_example(name))?;
    write(root, ".gitignore", templates::GITIGNORE)?;
    write(root, "rust-toolchain.toml", templates::RUST_TOOLCHAIN)?;
    write(root, "docker-compose.yml", &templates::docker_compose(name))?;
    write(root, "Dockerfile", templates::dockerfile())?;
    write(root, "README.md", &templates::readme(name, template))?;

    // Tiered settings (#87) — config/default.toml for shared knobs +
    // one <env>_settings.toml per tier. Runtime picks the tier from
    // RUSTANGO_ENV (default `dev`), so a fresh `cargo run` works
    // without any TOML edits.
    write(
        root,
        "config/default.toml",
        &templates::config_default_toml(name),
    )?;
    write(
        root,
        "config/dev_settings.toml",
        &templates::config_dev_settings_toml(name),
    )?;
    write(
        root,
        "config/staging_settings.toml",
        &templates::config_staging_settings_toml(name),
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

    // Tenant projects need the framework's registry+tenant bootstrap
    // migrations from day one — without them the very first
    // `cargo run -- migrate` reports "nothing to migrate" and then
    // errors when the tenant pass queries the (non-existent)
    // `rustango_orgs` table. Drop the same JSON `init-tenancy` would
    // produce so the project is `migrate`-ready out of the box.
    if matches!(template, Template::Tenant) {
        write(
            root,
            "migrations/0001_rustango_registry_initial.json",
            templates::BOOTSTRAP_REGISTRY_MIGRATION,
        )?;
        write(
            root,
            "migrations/0001_rustango_tenant_initial.json",
            templates::BOOTSTRAP_TENANT_MIGRATION,
        )?;
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
            let dep = template.rustango_features();
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
            let default_body = templates::config_default_toml(name);
            assert!(
                default_body.contains(name),
                "config_default_toml({name}) must mention the project name; got: {default_body}"
            );
            let dev = templates::config_dev_settings_toml(name);
            assert!(
                dev.contains("(dev)") || dev.contains("dev_settings"),
                "dev tier should be visually distinguishable; got: {dev}"
            );
            let staging = templates::config_staging_settings_toml(name);
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
            let dep = template.rustango_features();
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
        let body = templates::docker_compose("myapp");
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

    /// `.env.example` defaults must work out-of-box for `docker
    /// compose up -d` (host = `postgres`, bind = `0.0.0.0`). Users
    /// running cargo on the host edit `postgres` -> `localhost`.
    #[test]
    fn env_example_defaults_to_docker_friendly_values() {
        let body = templates::env_example("myapp");
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
