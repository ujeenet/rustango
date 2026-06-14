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
#[derive(Debug, Clone, Default)]
pub struct StartAppOptions {
    /// App module name. Becomes the `<base>/<app_name>/` directory
    /// (where `<base>` is `src/` by default — see [`Self::base_dir`]).
    /// Must be a valid Rust identifier (`[A-Za-z_][A-Za-z0-9_]*`).
    pub app_name: String,
    /// When `Some`, also write `<base>/bin/manage.rs` with this body.
    /// Skipped if the file already exists. `None` leaves manage.rs
    /// unchanged (the common case once a project has one).
    pub manage_bin: Option<&'static str>,
    /// Override the default `src/` directory the app lands in. Set
    /// to `Some(path)` for non-standard layouts — e.g.
    /// `examples/blog_demo` for an in-tree example, or `crates/web`
    /// for a workspace member with no `src/` parent. The scaffolder
    /// writes `<project_root>/<base_dir>/<app_name>/` and (when
    /// `manage_bin` is set) `<project_root>/<base_dir>/bin/manage.rs`.
    /// `None` keeps the v0.7 default of `src/`.
    pub base_dir: Option<PathBuf>,
    /// Crate-root identifier emitted into the scaffolded `use ...::sql`
    /// statements. `None` defaults to `"rustango"`; consumers that
    /// depend on a renamed dep (e.g. the future `rustango-orm` carve-
    /// out, [#144](https://github.com/ujeenet/rustango/issues/144))
    /// pass `Some("rustango_orm")` so the generated app compiles
    /// against the renamed crate. Phase 2 of issue
    /// [#145](https://github.com/ujeenet/rustango/issues/145).
    ///
    /// Note: only `use` paths are parameterized. The `#[rustango(...)]`
    /// derive attribute name stays literal because the proc-macro
    /// expects that token regardless of how the consumer renames the
    /// dep — same shape `rustango-renamed-smoke` proves under #142.
    pub crate_root: Option<String>,
}

/// Outcome of [`startapp`]: which files were written and which were
/// skipped because they already existed.
#[derive(Debug, Default)]
pub struct StartAppReport {
    /// Filesystem paths (relative to `project_root`) freshly written.
    pub written: Vec<String>,
    /// Filesystem paths that already existed and were left untouched.
    pub skipped: Vec<String>,
    /// Filesystem paths that were edited in place (slice 9.0g
    /// auto-mount). Distinct from `written` because the file already
    /// existed; we patched it to register the new app.
    pub patched: Vec<String>,
    /// Files we tried to patch but couldn't safely (couldn't find
    /// the expected anchor — user has hand-rolled their main.rs /
    /// urls.rs into a non-canonical shape). The CLI prints these
    /// with a "add this manually" hint.
    pub manual_steps: Vec<String>,
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
    let base_dir = opts
        .base_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("src"));
    let base_label = base_dir.display().to_string();
    let app_dir = project_root.join(&base_dir).join(&opts.app_name);
    if !app_dir.exists() {
        std::fs::create_dir_all(&app_dir)?;
    }

    let mod_body = render_mod_template(&opts.app_name);
    let singular = singularize(&opts.app_name);
    // Phase 2 of #145 — pick the crate root for `use ...::sql` paths.
    // `None` (the default) emits today's `use rustango::...` lines so
    // every existing call site stays bit-identical.
    let crate_root = opts.crate_root.as_deref().unwrap_or("rustango");
    let entries: [(&str, String); 5] = [
        ("mod.rs", mod_body),
        (
            "models.rs",
            render_models_template(&opts.app_name, crate_root),
        ),
        ("views.rs", VIEWS_TEMPLATE.into()),
        ("urls.rs", URLS_TEMPLATE.into()),
        ("tests.rs", render_tests_template(&singular, crate_root)),
    ];
    for (filename, body) in entries {
        let path = app_dir.join(filename);
        let rel = format!("{base_label}/{}/{}", opts.app_name, filename);
        write_or_skip(&path, &rel, &body, &mut report)?;
    }

    // Slice 9.0g — register the new app in the project's main.rs +
    // urls.rs. Conservative regex anchors with bail-out: if the file
    // doesn't have the expected aggregator pattern, we skip the edit
    // and surface a "add this manually" hint via report.manual_steps.
    let main_path = project_root.join(&base_dir).join("main.rs");
    let lib_path = project_root.join(&base_dir).join("lib.rs");
    let entry_path = if main_path.exists() {
        Some(main_path)
    } else if lib_path.exists() {
        Some(lib_path)
    } else {
        None
    };
    if let Some(path) = entry_path {
        let rel = path
            .strip_prefix(project_root)
            .unwrap_or(&path)
            .display()
            .to_string();
        match try_register_app_in_entry(&path, &opts.app_name)? {
            EntryEditOutcome::Patched => report.patched.push(rel),
            EntryEditOutcome::AlreadyRegistered => {} // silent — idempotent
            EntryEditOutcome::CouldNotFindAnchor => {
                report.manual_steps.push(format!(
                    "{rel}: add `mod {};` near the other `mod` declarations",
                    opts.app_name
                ));
            }
        }
    }

    let urls_path = project_root.join(&base_dir).join("urls.rs");
    if urls_path.exists() {
        let rel = urls_path
            .strip_prefix(project_root)
            .unwrap_or(&urls_path)
            .display()
            .to_string();
        match try_merge_app_into_urls(&urls_path, &opts.app_name)? {
            EntryEditOutcome::Patched => report.patched.push(rel),
            EntryEditOutcome::AlreadyRegistered => {}
            EntryEditOutcome::CouldNotFindAnchor => {
                report.manual_steps.push(format!(
                    "{rel}: add `.merge(crate::{}::urls::api())` to your aggregator router",
                    opts.app_name
                ));
            }
        }
    }

    if let Some(template) = opts.manage_bin {
        let bin_dir = project_root.join(&base_dir).join("bin");
        if !bin_dir.exists() {
            std::fs::create_dir_all(&bin_dir)?;
        }
        let path = bin_dir.join("manage.rs");
        let rel = format!("{base_label}/bin/manage.rs");
        write_or_skip(&path, &rel, template, &mut report)?;
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

/// Outcome of a single auto-edit attempt.
enum EntryEditOutcome {
    Patched,
    AlreadyRegistered,
    CouldNotFindAnchor,
}

/// Patch a project entry file (`src/main.rs` / `src/lib.rs`) to add
/// `mod <app_name>;`. Idempotent: if the line is already present,
/// returns `AlreadyRegistered` without rewriting. Anchors on:
///
///   1. An existing `mod <foo>;` line — appends the new `mod` after
///      the last consecutive `mod` declaration in the file.
///   2. Failing that, after the last `//!` doc comment block at the
///      top of the file.
///
/// If neither anchor is found (user has hand-rolled the layout into
/// something unusual), we bail out without modifying the file and
/// the caller surfaces a "add manually" hint.
fn try_register_app_in_entry(
    path: &Path,
    app_name: &str,
) -> Result<EntryEditOutcome, MigrateError> {
    let body = std::fs::read_to_string(path)?;
    let needle = format!("mod {app_name};");
    let needle_pub = format!("pub mod {app_name};");
    if body.contains(&needle) || body.contains(&needle_pub) {
        return Ok(EntryEditOutcome::AlreadyRegistered);
    }

    let lines: Vec<&str> = body.lines().collect();
    // Find the last contiguous run of `mod foo;` lines and insert
    // after it.
    let mod_anchor = lines
        .iter()
        .rposition(|l| l.trim_start().starts_with("mod ") && l.trim_end().ends_with(';'));
    let insert_at = if let Some(idx) = mod_anchor {
        idx + 1
    } else {
        // Fall back: after the leading docstring block (lines starting
        // with `//!` followed by a blank line).
        let mut i = 0;
        while i < lines.len() && lines[i].trim_start().starts_with("//!") {
            i += 1;
        }
        if i == 0 {
            return Ok(EntryEditOutcome::CouldNotFindAnchor);
        }
        // Skip a single blank line if present.
        if lines.get(i).is_some_and(|l| l.trim().is_empty()) {
            i + 1
        } else {
            i
        }
    };

    let mut out = String::with_capacity(body.len() + needle.len() + 1);
    for (i, line) in lines.iter().enumerate() {
        if i == insert_at {
            out.push_str(&needle);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
    }
    if insert_at >= lines.len() {
        out.push_str(&needle);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(EntryEditOutcome::Patched)
}

/// Patch `src/urls.rs` to add `.merge(crate::<app>::urls::api())` to
/// the project-root aggregator router. Idempotent. Anchors on the
/// pattern `Router::new()` followed by zero or more `.merge(...)` /
/// `.route(...)` / `.nest(...)` lines — appends the new `.merge` to
/// the end of that chain (just before the trailing `}` of the body).
///
/// Bail-out: if the file doesn't have a `Router::new()` call, return
/// `CouldNotFindAnchor` and let the caller surface a manual-step hint.
fn try_merge_app_into_urls(path: &Path, app_name: &str) -> Result<EntryEditOutcome, MigrateError> {
    let body = std::fs::read_to_string(path)?;
    let merge_call = format!(".merge(crate::{app_name}::urls::api())");
    if body.contains(&merge_call) {
        return Ok(EntryEditOutcome::AlreadyRegistered);
    }

    // Find the line that creates the Router. We append the .merge
    // immediately after this line — the user can re-indent if they
    // prefer a different call-chain style, but this works as a
    // valid expression continuation.
    let lines: Vec<&str> = body.lines().collect();
    let anchor = lines.iter().rposition(|l| l.contains("Router::new()"));
    let Some(idx) = anchor else {
        return Ok(EntryEditOutcome::CouldNotFindAnchor);
    };

    // Detect indentation of the anchor line so the inserted line
    // looks at home next to siblings.
    let indent: String = lines[idx]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let inserted_indent = format!("{indent}    ");
    let inserted = format!("{inserted_indent}{merge_call}");

    let mut out = String::with_capacity(body.len() + inserted.len() + 1);
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push('\n');
        if i == idx {
            out.push_str(&inserted);
            out.push('\n');
        }
    }
    std::fs::write(path, out)?;
    Ok(EntryEditOutcome::Patched)
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
         pub mod views;\n\
         \n\
         #[cfg(test)]\n\
         mod tests;\n",
    )
}

/// Default `models.rs` body — a single starter model named after
/// the singularized app name (e.g. `startapp posts` → `pub struct Post`
/// on table `"post"`; `startapp blog` → `pub struct Blog` on table
/// `"blog"`). Parameterising the struct + table name avoids colliding
/// with the project-root `Item` example or with another app's `Item`.
///
/// The starter model is admin-visible out of the box: rustango defaults
/// `permissions = true`, so [`tenancy::permissions::auto_create_permissions`]
/// seeds the four CRUD codenames during `migrate`. After running
/// `cargo run -- migrate`, a freshly-created superuser sees the model
/// in the admin sidebar without any manual permission-grant step.
///
/// Singularization is conservative: trailing `s` is stripped on names
/// of length ≥ 5 (so `posts → post`, `comments → comment`, but
/// `news` / `feed` / `cms` stay as-is). Users can rename either the
/// struct or the `table = "..."` literal freely; the macro reads them
/// independently.
fn render_models_template(app_name: &str, crate_root: &str) -> String {
    let singular = singularize(app_name);
    let struct_name = pascal_case(&singular);
    format!(
        "//! App models — every `#[derive(Model)]` lives here.
//!
//! Adding a struct here makes it admin-visible automatically: the
//! macro populates the `inventory` registry that
//! `{crate_root}::admin::router(pool)` walks. The four standard CRUD
//! permission codenames (`{singular}.add`, `.change`, `.delete`,
//! `.view`) are seeded by `auto_create_permissions` during the
//! first `migrate`, so non-superuser tenant users see this model
//! once granted an appropriate role.
//!
//! Rename `{struct_name}` / `\"{singular}\"` to suit your domain;
//! the table name and struct identifier are independent.

use {crate_root}::sql::Auto;
use {crate_root}::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = \"{singular}\",
    display = \"name\",
    admin(
        list_display = \"name, active, created_at\",
        search_fields = \"name\",
        ordering = \"-created_at\",
    )
)]
pub struct {struct_name} {{
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    pub active: bool,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}}
"
    )
}

/// Strip a trailing `s` for the common English-plural app names
/// (`posts`, `comments`, `users`). Conservative — leaves anything
/// shorter than 5 chars or not ending in `s` untouched. Users can
/// always rename the struct + table literal manually if the
/// heuristic guesses wrong (e.g. `categories` → `categorie`).
fn singularize(name: &str) -> String {
    if name.len() >= 5 && name.ends_with('s') && !name.ends_with("ss") && !name.ends_with("us") {
        name[..name.len() - 1].to_owned()
    } else {
        name.to_owned()
    }
}

fn pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase())
                .into_iter()
                .chain(chars.flat_map(char::to_lowercase))
                .collect::<String>()
        })
        .collect::<String>()
}

/// Default `views.rs` body — placeholder handler.
///
/// Project-root `src/views.rs` already ships `index` + `healthz` for
/// `GET /` and `GET /healthz`, so the new app starts empty (one stub
/// handler the user can replace) to avoid duplicate-route panics
/// when the scaffolder auto-merges the new app's router.
const VIEWS_TEMPLATE: &str = "//! App views — request handlers (Django-style \"views\").
//!
//! Each handler is a stateless async fn; `urls.rs` mounts them
//! under their HTTP paths. For pure-CRUD admin needs you don't
//! need any custom views — `rustango::admin::router(pool)` covers
//! that. Replace the stub below with your own handlers and add
//! corresponding `.route(...)` lines in `urls.rs`.

use axum::response::Html;

/// `GET /<app-prefix>/hello` — placeholder. Wire the actual path
/// in `urls.rs` once you decide on the app's URL prefix.
pub async fn hello() -> Html<&'static str> {
    Html(\"<h1>hello from your new app</h1>\")
}
";

/// Default `urls.rs` body for an app — exposes a stateless
/// `Router<()>` via `pub fn api()` so the project-root urls.rs
/// aggregator can `.merge(crate::<app>::urls::api())` it. Slice 9.0g
/// shape — the project's main.rs / Builder mounts admin + tenant
/// dispatch separately, so the app router stays focused on the
/// custom routes the user adds.
const URLS_TEMPLATE: &str = "//! App URL routing.
//!
//! `pub fn api() -> Router<()>` — every route this app exposes.
//! The project-root `src/urls.rs` aggregator calls
//! `.merge(crate::<this_app>::urls::api())` so these routes show up
//! at the project's root. Handlers can take
//! `rustango::extractors::Tenant` (in tenancy projects) or extract
//! state via axum's normal `State<...>` mechanism.
//!
//! Starts empty — uncomment the example or add your own routes.
//! Defining `/` or `/healthz` here would clash with the project-
//! root router, so prefer an app-specific prefix like `/blog/...`.

use axum::Router;

#[allow(unused_imports)]
use axum::routing::get;
#[allow(unused_imports)]
use super::views;

pub fn api() -> Router<()> {
    Router::new()
        // .route(\"/blog/hello\", get(views::hello))
}
";

/// Default `tests.rs` body — generated per-app so the inventory
/// smoke test can reference the starter model's table by name.
fn render_tests_template(singular_table: &str, crate_root: &str) -> String {
    // This file is already pulled in as the `tests` module via
    // `#[cfg(test)] mod tests;` in `mod.rs`, so the body lives at module
    // scope here — NOT wrapped in another `mod tests {{ }}` (that would
    // nest to `<app>::tests::tests` and break the `super::` paths).
    format!(
        "//! App-level integration tests.
//!
//! Run with `cargo test`. Uses `{crate_root}::test_client::TestClient` to
//! exercise the app's router in-process — no network, no real socket.

use super::urls::api;

/// Smoke test — the empty router builds without panicking.
/// Replace with real route assertions once you add `.route(...)`
/// lines in `urls.rs`.
#[tokio::test]
async fn router_builds() {{
    let _router = api();
}}

/// Smoke test — every `#[derive(Model)]` in `models.rs` registers
/// itself in `inventory` at link time. The auto-admin walks that
/// registry, so seeing your model here is the canonical
/// confirmation that the admin will pick it up.
///
/// If you rename the starter model's `table = \"...\"`, update
/// the literal below.
#[test]
fn starter_model_registered_in_inventory() {{
    use {crate_root}::core::ModelEntry;
    let tables: Vec<&'static str> = {crate_root}::inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema.table)
        .collect();
    assert!(
        tables.iter().any(|t| *t == \"{singular_table}\"),
        \"`{singular_table}` missing from inventory; tables: {{tables:?}}\",
    );
}}
"
    )
}

/// Single-tenant `manage.rs` template — wires the standard
/// `rustango::migrate::manage::run` dispatcher. Pass to
/// [`StartAppOptions::manage_bin`] when bootstrapping a non-tenancy
/// project.
pub const SINGLE_TENANT_MANAGE_BIN: &str =
    "//! Generated by `manage startapp --with-manage-bin`. Edit freely.
//!
//! UX: `cargo run -- migrate`,
//! `cargo run -- makemigrations`, etc. The dispatcher is
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
                ..Default::default()
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
                "src/blog/tests.rs",
            ]
        );
        for f in ["mod.rs", "models.rs", "views.rs", "urls.rs", "tests.rs"] {
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
                ..Default::default()
            },
        )
        .unwrap();
        let second = startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(second.written.is_empty());
        assert_eq!(second.skipped.len(), 5);
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
                ..Default::default()
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
                ..Default::default()
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
        // tests.rs is cfg-gated
        assert!(body.contains("#[cfg(test)]"));
        assert!(body.contains("mod tests;"));
    }

    #[test]
    fn singularize_strips_trailing_s_only_for_long_words() {
        assert_eq!(singularize("posts"), "post");
        assert_eq!(singularize("comments"), "comment");
        assert_eq!(singularize("users"), "user");
        // Short words (< 5 chars) stay as-is to avoid mangling
        // names like `cms` / `dms`.
        assert_eq!(singularize("dms"), "dms");
        // Words ending in `ss` / `us` / non-`s` are untouched.
        assert_eq!(singularize("address"), "address");
        assert_eq!(singularize("bus"), "bus");
        assert_eq!(singularize("blog"), "blog");
        assert_eq!(singularize("news"), "news");
    }

    #[test]
    fn rendered_models_template_singularizes_app_name() {
        let body = render_models_template("posts", "rustango");
        assert!(
            body.contains("pub struct Post {"),
            "expected singular `Post` struct, got: {body}"
        );
        assert!(
            body.contains("table = \"post\""),
            "expected singular table name, got: {body}"
        );
    }

    #[test]
    fn rendered_models_template_includes_admin_config_and_created_at() {
        let body = render_models_template("blog", "rustango");
        assert!(
            body.contains("admin("),
            "expected admin(...) config block, got: {body}"
        );
        assert!(
            body.contains("list_display = \"name, active, created_at\""),
            "expected list_display, got: {body}"
        );
        assert!(
            body.contains("created_at: Auto<chrono::DateTime<chrono::Utc>>"),
            "expected created_at field wrapped in Auto<...>, got: {body}"
        );
        assert!(
            body.contains("auto_now_add"),
            "expected auto_now_add, got: {body}"
        );
    }

    #[test]
    fn rendered_tests_template_asserts_inventory_registration() {
        let body = render_tests_template("post", "rustango");
        assert!(
            body.contains("starter_model_registered_in_inventory"),
            "expected inventory smoke test, got: {body}"
        );
        assert!(
            body.contains("\"post\""),
            "expected singular table literal in test, got: {body}"
        );
    }

    #[test]
    fn rendered_models_template_threads_default_crate_root() {
        // Phase 2 of #145 — without an explicit override the emit
        // mentions `rustango::` exactly as it did pre-#145. This is
        // the regression guard for every existing call site.
        let body = render_models_template("posts", "rustango");
        assert!(
            body.contains("use rustango::sql::Auto;"),
            "default crate root must emit `use rustango::sql::Auto;`, got: {body}"
        );
        assert!(
            body.contains("use rustango::Model;"),
            "default crate root must emit `use rustango::Model;`, got: {body}"
        );
    }

    #[test]
    fn rendered_models_template_threads_renamed_crate_root() {
        // Phase 2 of #145 — passing `"rustango_orm"` produces source
        // a bare-ORM consumer (post-#144) can compile against. The
        // `#[rustango(...)]` derive attribute name stays literal —
        // it's hard-coded in the proc-macro regardless of the
        // consumer's Cargo.toml package rename. Same shape
        // `rustango-renamed-smoke` proves under #142.
        let body = render_models_template("posts", "rustango_orm");
        assert!(
            body.contains("use rustango_orm::sql::Auto;"),
            "renamed crate root must propagate, got: {body}"
        );
        assert!(
            body.contains("use rustango_orm::Model;"),
            "renamed crate root must propagate, got: {body}"
        );
        assert!(
            !body.contains("use rustango::"),
            "after rename no bare `use rustango::` should remain, got: {body}"
        );
        assert!(
            body.contains("#[rustango("),
            "derive attribute name stays literal across renames, got: {body}"
        );
    }

    #[test]
    fn rendered_tests_template_threads_crate_root() {
        // Default path stays bit-identical.
        let default_body = render_tests_template("post", "rustango");
        assert!(default_body.contains("use rustango::core::ModelEntry;"));

        // Renamed path picks up the new root.
        let renamed_body = render_tests_template("post", "rustango_orm");
        assert!(renamed_body.contains("use rustango_orm::core::ModelEntry;"));
        assert!(!renamed_body.contains("use rustango::"));
    }

    #[test]
    fn startapp_threads_crate_root_through_models() {
        // End-to-end: `startapp` with `crate_root: Some("rustango_orm")`
        // produces an app whose `models.rs` compiles against the
        // renamed crate. The integration check at the byte level so a
        // future renderer refactor can't quietly drop the override.
        let root = fresh_root("crate_root_e2e");
        startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                crate_root: Some("rustango_orm".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let body =
            std::fs::read_to_string(root.join("src").join("blog").join("models.rs")).unwrap();
        assert!(body.contains("use rustango_orm::sql::Auto;"));
        assert!(body.contains("use rustango_orm::Model;"));
        // Per-app `tests.rs` also picks up the rename.
        let tests_body =
            std::fs::read_to_string(root.join("src").join("blog").join("tests.rs")).unwrap();
        assert!(tests_body.contains("use rustango_orm::core::ModelEntry;"));
    }

    #[test]
    fn full_startapp_produces_singularized_polished_model() {
        // End-to-end: invoke `startapp` against a temp project, read
        // the materialized `models.rs` and `tests.rs`, assert the v0.28.2
        // polish made it through (singularization, admin config, smoke
        // test referencing the same singular table).
        let root = fresh_root("polished_e2e");
        let _ = startapp(
            &root,
            &StartAppOptions {
                app_name: "posts".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let models =
            std::fs::read_to_string(root.join("src").join("posts").join("models.rs")).unwrap();
        assert!(models.contains("pub struct Post {"), "models: {models}");
        assert!(models.contains("table = \"post\""), "models: {models}");
        assert!(models.contains("admin("), "models: {models}");
        assert!(models.contains("auto_now_add"), "models: {models}");
        let tests =
            std::fs::read_to_string(root.join("src").join("posts").join("tests.rs")).unwrap();
        assert!(
            tests.contains("starter_model_registered_in_inventory"),
            "tests: {tests}"
        );
        assert!(tests.contains("\"post\""), "tests: {tests}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_mount_inserts_mod_after_existing_mods() {
        let root = fresh_root("automount_main");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let main = src.join("main.rs");
        std::fs::write(
            &main,
            "//! example main.rs\n\
             \n\
             mod blog;\n\
             mod views;\n\
             \n\
             fn main() {}\n",
        )
        .unwrap();
        let report = startapp(
            &root,
            &StartAppOptions {
                app_name: "shop".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let body = std::fs::read_to_string(&main).unwrap();
        assert!(body.contains("mod shop;"), "body was {body}");
        assert!(report.patched.iter().any(|p| p.contains("main.rs")));
        // Re-running is idempotent; no double-add.
        let report2 = startapp(
            &root,
            &StartAppOptions {
                app_name: "shop".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report2.patched.len(), 0, "second run should not patch");
        let body2 = std::fs::read_to_string(&main).unwrap();
        assert_eq!(body2.matches("mod shop;").count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_mount_appends_merge_call_to_urls_router() {
        let root = fresh_root("automount_urls");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            src.join("urls.rs"),
            "use axum::Router;\n\
             pub fn api() -> Router<()> {\n    \
                 Router::new()\n\
             }\n",
        )
        .unwrap();
        let report = startapp(
            &root,
            &StartAppOptions {
                app_name: "shop".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let body = std::fs::read_to_string(src.join("urls.rs")).unwrap();
        assert!(
            body.contains(".merge(crate::shop::urls::api())"),
            "urls.rs was: {body}"
        );
        assert!(report.patched.iter().any(|p| p.contains("urls.rs")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_mount_emits_manual_step_when_no_anchor() {
        let root = fresh_root("automount_bail");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // urls.rs without an axum router-construction anchor — should
        // bail out and emit a manual-step hint.
        std::fs::write(
            src.join("urls.rs"),
            "// hand-rolled aggregator with no recognisable anchor\npub fn api() {}\n",
        )
        .unwrap();
        std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        let report = startapp(
            &root,
            &StartAppOptions {
                app_name: "shop".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            report.manual_steps.iter().any(|h| h.contains("urls.rs")),
            "expected a manual-step hint for urls.rs, got: {:?}",
            report.manual_steps
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
