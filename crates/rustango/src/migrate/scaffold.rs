//! Project scaffolder — Django's `startapp` for rustango.
//!
//! [`startapp`] writes a Django-shape app module into a project's
//! `src/` tree:
//!
//! ```text
//! src/<app>/
//!   mod.rs       — re-exports models / views / urls
//!   models.rs    — #[derive(Model)] structs (admin-visible automatically)
//!   views.rs     — request handlers (Django-style "views")
//!   urls.rs      — Router builder mapping paths → views
//! ```
//!
//! Idempotent: every file that already exists is reported in
//! [`StartAppReport::skipped`] and left untouched. Parent directories
//! are created on demand.
//!
//! The optional `manage_bin` template, when set, additionally writes
//! `src/bin/manage.rs` — the 5-line dispatcher boilerplate. Caller
//! decides which template to pass; this crate ships
//! [`SINGLE_TENANT_MANAGE_BIN`] for the standard
//! `rustango::migrate::manage::run` flow, and
//! `crate::tenancy` ships its own tenancy-aware template that
//! wires `crate::tenancy::manage::run` instead.

use std::path::{Path, PathBuf};

use super::error::MigrateError;

/// Options for [`startapp`].
#[derive(Debug, Clone)]
pub struct StartAppOptions {
    /// App module name. Becomes the `src/<app_name>/` directory.
    /// Must be a valid Rust identifier (`[A-Za-z_][A-Za-z0-9_]*`).
    pub app_name: String,
    /// When `Some`, also write `src/bin/manage.rs` with this body.
    /// Skipped if the file already exists. `None` leaves manage.rs
    /// unchanged (the common case once a project has one).
    pub manage_bin: Option<&'static str>,
}

/// Outcome of [`startapp`]: which files were written and which were
/// skipped because they already existed.
#[derive(Debug, Default)]
pub struct StartAppReport {
    /// Filesystem paths (relative to `project_root`) freshly written.
    pub written: Vec<String>,
    /// Filesystem paths that already existed and were left untouched.
    pub skipped: Vec<String>,
}

/// Materialize a Django-shape app module into `project_root/src/<app>/`.
///
/// `project_root` is typically the directory containing `Cargo.toml`.
/// The function does **not** parse Cargo.toml or modify it — adding
/// `mod <app_name>;` to `src/lib.rs` or `src/main.rs` is the user's
/// step.
///
/// # Errors
/// Returns [`MigrateError::Validation`] for an invalid app name,
/// or [`MigrateError::Io`] for any filesystem failure.
pub fn startapp(
    project_root: &Path,
    opts: &StartAppOptions,
) -> Result<StartAppReport, MigrateError> {
    validate_app_name(&opts.app_name)?;

    let mut report = StartAppReport::default();
    let app_dir = project_root.join("src").join(&opts.app_name);
    if !app_dir.exists() {
        std::fs::create_dir_all(&app_dir)?;
    }

    let mod_body = render_mod_template(&opts.app_name);
    let entries: [(&str, String); 4] = [
        ("mod.rs", mod_body),
        ("models.rs", MODELS_TEMPLATE.into()),
        ("views.rs", VIEWS_TEMPLATE.into()),
        ("urls.rs", URLS_TEMPLATE.into()),
    ];
    for (filename, body) in entries {
        let path = app_dir.join(filename);
        let rel = format!("src/{}/{}", opts.app_name, filename);
        write_or_skip(&path, &rel, &body, &mut report)?;
    }

    if let Some(template) = opts.manage_bin {
        let bin_dir = project_root.join("src").join("bin");
        if !bin_dir.exists() {
            std::fs::create_dir_all(&bin_dir)?;
        }
        let path = bin_dir.join("manage.rs");
        write_or_skip(&path, "src/bin/manage.rs", template, &mut report)?;
    }

    Ok(report)
}

fn write_or_skip(
    path: &PathBuf,
    rel: &str,
    body: &str,
    report: &mut StartAppReport,
) -> Result<(), MigrateError> {
    if path.exists() {
        report.skipped.push(rel.to_owned());
        return Ok(());
    }
    std::fs::write(path, body)?;
    report.written.push(rel.to_owned());
    Ok(())
}

fn validate_app_name(name: &str) -> Result<(), MigrateError> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if !valid {
        return Err(MigrateError::Validation(format!(
            "app name `{name}` is not a valid Rust identifier — \
             must match [A-Za-z_][A-Za-z0-9_]*"
        )));
    }
    Ok(())
}

fn render_mod_template(app_name: &str) -> String {
    format!(
        "//! `{app_name}` — Django-shape app module.\n\
         //!\n\
         //! Add `mod {app_name};` (or `pub mod {app_name};`) to your\n\
         //! `src/main.rs` / `src/lib.rs` so these submodules are\n\
         //! pulled into the binary's `inventory` registry.\n\
         \n\
         pub mod models;\n\
         pub mod urls;\n\
         pub mod views;\n",
    )
}

/// Default `models.rs` body — a single starter `Item` model.
const MODELS_TEMPLATE: &str = "//! App models — every `#[derive(Model)]` lives here.
//!
//! Adding a struct here makes it admin-visible automatically: the
//! macro populates the `inventory` registry that
//! `rustango::admin::router(pool)` walks. No per-model registration
//! step.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = \"item\", display = \"name\")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub active: bool,
}
";

/// Default `views.rs` body — landing page + healthz.
const VIEWS_TEMPLATE: &str = "//! App views — request handlers (Django-style \"views\").
//!
//! Each handler is a stateless async fn; `urls.rs` mounts them
//! under their HTTP paths. For pure-CRUD admin needs you don't
//! need any custom views — `rustango::admin::router(pool)` covers
//! that. The handlers below are stubs you can replace.

use axum::response::Html;

/// `GET /` — landing page with a link into the auto-admin.
pub async fn index() -> Html<&'static str> {
    Html(
        \"<!doctype html>\\n\\
         <title>rustango app</title>\\n\\
         <h1>Hello from rustango!</h1>\\n\\
         <p>The auto-admin is at <a href=\\\"/admin\\\">/admin</a>.</p>\",
    )
}

/// `GET /healthz` — liveness probe.
pub async fn healthz() -> &'static str {
    \"ok\"
}
";

/// Default `urls.rs` body — wires the views + nests the auto-admin.
const URLS_TEMPLATE: &str = "//! App URL routing.
//!
//! Single function `router(pool) -> Router` that wires every HTTP
//! path the app exposes. The auto-admin mounts under `/admin`;
//! custom views from `views.rs` mount alongside.

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::sql::sqlx::PgPool;

use super::views;

pub fn router(pool: PgPool) -> Router {
    let admin = admin::Builder::new(pool.clone()).build();
    Router::new()
        .route(\"/\", get(views::index))
        .route(\"/healthz\", get(views::healthz))
        .with_state(pool)
        .nest(\"/admin\", admin)
}
";

/// Single-tenant `manage.rs` template — wires the standard
/// `rustango::migrate::manage::run` dispatcher. Pass to
/// [`StartAppOptions::manage_bin`] when bootstrapping a non-tenancy
/// project.
pub const SINGLE_TENANT_MANAGE_BIN: &str =
    "//! Generated by `manage startapp --with-manage-bin`. Edit freely.
//!
//! UX: `cargo run --bin manage -- migrate`,
//! `cargo run --bin manage -- makemigrations`, etc. The dispatcher is
//! defined in `rustango::migrate::manage`; this binary just hands it
//! the pool and argv.

use rustango::sql::sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Pull your models into this binary so `inventory` registers
    // them — replace the placeholder line below with whatever fits
    // your project layout. For the `manage startapp <name>` shape:
    //   #[allow(unused_imports)]
    //   use super::<name>::models::*;
    // For a top-level src/models.rs:
    //   #[allow(unused_imports)]
    //   use super::models::*;

    let pool = PgPool::connect(&std::env::var(\"DATABASE_URL\")?).await?;
    let dir: &std::path::Path = \"./migrations\".as_ref();
    rustango::migrate::manage::run(&pool, dir, std::env::args().skip(1)).await?;
    Ok(())
}
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fresh_root(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("rustango_scaffold_{label}_{pid}_{n}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn writes_app_files_into_src_subdir() {
        let root = fresh_root("writes_app");
        let report = startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                manage_bin: None,
            },
        )
        .unwrap();
        assert_eq!(report.skipped, Vec::<String>::new());
        assert_eq!(
            report.written,
            vec![
                "src/blog/mod.rs",
                "src/blog/models.rs",
                "src/blog/views.rs",
                "src/blog/urls.rs",
            ]
        );
        for f in ["mod.rs", "models.rs", "views.rs", "urls.rs"] {
            let p = root.join("src").join("blog").join(f);
            assert!(p.exists(), "{}", p.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn second_run_skips_existing_files() {
        let root = fresh_root("idempotent");
        let _ = startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                manage_bin: None,
            },
        )
        .unwrap();
        let second = startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                manage_bin: None,
            },
        )
        .unwrap();
        assert!(second.written.is_empty());
        assert_eq!(second.skipped.len(), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manage_bin_template_writes_src_bin_manage_rs() {
        let root = fresh_root("manage_bin");
        let report = startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                manage_bin: Some(SINGLE_TENANT_MANAGE_BIN),
            },
        )
        .unwrap();
        assert!(report.written.contains(&"src/bin/manage.rs".to_owned()));
        let body = std::fs::read_to_string(root.join("src/bin/manage.rs")).unwrap();
        assert!(body.contains("rustango::migrate::manage::run"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_app_name_is_rejected() {
        let root = fresh_root("invalid");
        let err = startapp(
            &root,
            &StartAppOptions {
                app_name: "1bad-name".into(),
                manage_bin: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, MigrateError::Validation(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rendered_mod_template_pulls_in_three_submodules() {
        let body = render_mod_template("shop");
        assert!(body.contains("`shop`"));
        assert!(body.contains("pub mod models;"));
        assert!(body.contains("pub mod urls;"));
        assert!(body.contains("pub mod views;"));
    }
}
