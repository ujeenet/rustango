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
//!                  tenancy_manage CLI scaffolded into src/bin/manage.rs.
//!
//! Each template writes a self-contained Cargo project into `<cwd>/<name>/`:
//!
//!   <name>/
//!     Cargo.toml
//!     .env.example
//!     docker-compose.yml
//!     migrations/
//!     src/
//!       main.rs
//!       models.rs
//!       views.rs
//!       urls.rs
//!     src/bin/manage.rs
//!
//! Once written, the user typically runs:
//!
//!   $ cd <name> && docker compose up -d
//!   $ cargo run --bin manage -- migrate
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

    fn rustango_features(self) -> &'static str {
        match self {
            Self::Api => r#"{ version = "0.8", default-features = false, features = ["postgres"] }"#,
            Self::Fullstack => r#""0.8""#,
            Self::Tenant => r#"{ version = "0.8", features = ["tenancy"] }"#,
        }
    }
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
    println!("  docker compose up -d");
    println!("  cargo run --bin manage -- migrate");
    println!("  cargo run");
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
    let name = name.ok_or_else(|| "missing project name (e.g. `cargo rustango new myapp`)".to_owned())?;
    Ok(NewArgs { name, template })
}

fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
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
    write(root, ".env.example", templates::ENV_EXAMPLE)?;
    write(root, ".gitignore", templates::GITIGNORE)?;
    write(root, "rust-toolchain.toml", templates::RUST_TOOLCHAIN)?;
    write(root, "docker-compose.yml", &templates::docker_compose(name))?;
    write(root, "README.md", &templates::readme(name, template))?;

    fs::create_dir_all(root.join("migrations"))
        .map_err(|e| format!("create migrations/: {e}"))?;
    fs::create_dir_all(root.join("src/bin"))
        .map_err(|e| format!("create src/bin/: {e}"))?;

    write(root, "src/main.rs", templates::main_rs(template))?;
    write(root, "src/models.rs", &templates::models_rs(template))?;
    write(root, "src/views.rs", templates::VIEWS_RS)?;
    write(root, "src/urls.rs", &templates::urls_rs(template))?;
    write(root, "src/bin/manage.rs", &templates::manage_rs(template))?;

    // Tenant projects need the framework's registry+tenant bootstrap
    // migrations from day one — without them the very first
    // `cargo run --bin manage -- migrate` reports "nothing to migrate"
    // and then errors when the tenant pass queries the (non-existent)
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
