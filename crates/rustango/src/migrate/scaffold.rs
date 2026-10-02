//! Project scaffolder: lay out a new app module from a template.
//!
//! [`startapp`] writes an app module into a project's `src/` tree:
//!
//! ```text
//! src/<app>/
//!   mod.rs       — re-exports models / views / urls
//!   models.rs    — #[derive(Model)] structs (admin-visible automatically)
//!   views.rs     — request handlers ("views")
//!   urls.rs      — Router builder mapping paths → views
//! ```
//!
//! Safe to re-run: an existing file is listed in
//! [`StartAppReport::skipped`] and left alone. Parent directories are
//! created as needed.
//!
//! Set `manage_bin` to also write `src/bin/manage.rs`. Use
//! [`SINGLE_TENANT_MANAGE_BIN`] for the plain
//! `rustango::migrate::manage::run` flow; `crate::tenancy` ships a
//! tenancy-aware template of its own.

use std::path::{Path, PathBuf};

use super::error::MigrateError;

/// Options for [`startapp`].
#[derive(Debug, Clone, Default)]
pub struct StartAppOptions {
    /// App module name, which becomes the `<base>/<app_name>/`
    /// directory. Must be a valid Rust identifier. See
    /// [`Self::base_dir`] for `<base>`.
    pub app_name: String,
    /// When `Some`, also write `<base>/bin/manage.rs` with this body,
    /// unless that file already exists.
    pub manage_bin: Option<&'static str>,
    /// Directory the app lands in, relative to `project_root`.
    /// `None` means `src/`. Set it for an unusual layout, such as an
    /// in-tree example or a workspace member with no `src/` parent.
    pub base_dir: Option<PathBuf>,
    /// Crate name to write into the generated `use ...` lines.
    /// `None` means `"rustango"`. Set it when the project depends on
    /// a renamed crate, so the generated app still compiles.
    ///
    /// Only `use` paths change. The `#[rustango(...)]` attribute name
    /// stays as it is, because the proc-macro expects that token
    /// whatever the crate is called.
    pub crate_root: Option<String>,
}

/// Outcome of [`startapp`]: which files were written and which were
/// skipped because they already existed.
#[derive(Debug, Default)]
pub struct StartAppReport {
    /// Paths, relative to `project_root`, that were newly written.
    pub written: Vec<String>,
    /// Paths that already existed and were left alone.
    pub skipped: Vec<String>,
    /// Existing files edited in place to register the new app.
    pub patched: Vec<String>,
    /// Files the scaffolder could not patch safely, because their
    /// layout does not match the expected shape. The CLI prints
    /// these with an "add this manually" hint.
    pub manual_steps: Vec<String>,
}

/// Write an app module into `project_root/src/<app>/`.
///
/// `project_root` is usually the directory holding `Cargo.toml`. This
/// function never reads or edits `Cargo.toml`.
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
    // Crate root for the generated `use ...` paths.
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

    // Register the new app in the project's entry file. If the file
    // does not have the expected shape, skip the edit and add an
    // "add this manually" hint to `report.manual_steps`.
    //
    // `lib.rs` is tried first on purpose. The app's modules must live
    // in the library so binaries under `src/bin/` can reach them; a
    // `mod <app>;` in `main.rs` is invisible to the library's
    // `urls.rs`. Older projects with only a `main.rs` still work.
    let main_path = project_root.join(&base_dir).join("main.rs");
    let lib_path = project_root.join(&base_dir).join("lib.rs");
    let entry_path = if lib_path.exists() {
        Some(lib_path)
    } else if main_path.exists() {
        Some(main_path)
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
                // Suggest the same spelling the patcher would use, so
                // the manual edit lands where urls.rs can see it.
                let vis = if path.file_name().is_some_and(|f| f == "lib.rs") {
                    "pub mod"
                } else {
                    "mod"
                };
                report.manual_steps.push(format!(
                    "{rel}: add `{vis} {};` near the other `mod` declarations",
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

/// Add `mod <app_name>;` to `src/main.rs` or `src/lib.rs`. Returns
/// `AlreadyRegistered` and rewrites nothing if the line is there.
///
/// The new line goes after the last `mod ...;` declaration, or after
/// the leading `//!` block if there is none. With neither anchor the
/// file is left untouched and the caller prints a manual-step hint.
fn try_register_app_in_entry(
    path: &Path,
    app_name: &str,
) -> Result<EntryEditOutcome, MigrateError> {
    let body = std::fs::read_to_string(path)?;
    // In a library the module must be `pub`, or the binaries that
    // reach it as `my_app::<name>` cannot see it. A binary-only
    // layout wants a plain `mod`.
    let is_lib = path.file_name().is_some_and(|f| f == "lib.rs");
    let needle = if is_lib {
        format!("pub mod {app_name};")
    } else {
        format!("mod {app_name};")
    };
    if body.contains(&format!("mod {app_name};")) {
        return Ok(EntryEditOutcome::AlreadyRegistered);
    }

    let lines: Vec<&str> = body.lines().collect();
    // Insert after the last `mod foo;` or `pub mod foo;` line. Both
    // spellings matter: a generated `lib.rs` uses `pub mod`
    // throughout, and matching only `mod ` would miss every line.
    let mod_anchor = lines.iter().rposition(|l| {
        let t = l.trim_start();
        (t.starts_with("mod ") || t.starts_with("pub mod ")) && l.trim_end().ends_with(';')
    });
    let insert_at = if let Some(idx) = mod_anchor {
        idx + 1
    } else {
        // Fall back to just after the leading `//!` block.
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

/// Add `.merge(crate::<app>::urls::api())` to the root router in
/// `src/urls.rs`, just after the `Router::new()` line. Safe to
/// re-run.
///
/// Without a `Router::new()` call, returns `CouldNotFindAnchor` and
/// the caller prints a manual-step hint.
fn try_merge_app_into_urls(path: &Path, app_name: &str) -> Result<EntryEditOutcome, MigrateError> {
    let body = std::fs::read_to_string(path)?;
    let merge_call = format!(".merge(crate::{app_name}::urls::api())");
    if body.contains(&merge_call) {
        return Ok(EntryEditOutcome::AlreadyRegistered);
    }

    // Append the `.merge` right after the line that builds the
    // Router. It is a valid continuation of the call chain; the user
    // can re-indent it to taste.
    let lines: Vec<&str> = body.lines().collect();
    let anchor = lines.iter().rposition(|l| l.contains("Router::new()"));
    let Some(idx) = anchor else {
        return Ok(EntryEditOutcome::CouldNotFindAnchor);
    };

    // Match the anchor line's indentation.
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
        "//! `{app_name}` — app module.\n\
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

/// Default `models.rs` body: one starter model named after the
/// singular form of the app name, so `startapp posts` gives
/// `pub struct Post` on table `"post"`. Naming it after the app keeps
/// it from clashing with another app's starter model.
///
/// The model is admin-visible right away. `permissions = true` is the
/// default, so `migrate` seeds its four CRUD codenames and a new
/// superuser sees it in the admin sidebar with no extra step.
///
/// See [`singularize`] for how the singular name is picked.
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

/// Strip a trailing `s` from a plural app name: `posts` gives
/// `post`. Deliberately simple, so a name under 5 characters or one
/// that does not end in `s` is left alone. It guesses wrong on words
/// like `categories`; rename the struct and table by hand then.
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

/// Default `views.rs` body: one stub handler.
///
/// The project root already serves `GET /` and `GET /healthz`, so a
/// new app starts with no real routes. Adding them here would panic
/// on a duplicate route when the scaffolder merges the app's router.
const VIEWS_TEMPLATE: &str = "//! App views — request handlers (\"views\").
//!
//! Each handler is a stateless async fn; `urls.rs` mounts them
//! under their HTTP paths. For pure-CRUD admin needs you don't
//! need any custom views — `rustango::admin::router(pool)` covers
//! that. Replace the stub below with your own handlers and add
//! corresponding `.route(...)` lines in `urls.rs`.

use axum::response::Html;

/// `GET /<app-prefix>/hello` — placeholder. Wire the actual path
/// in `urls.rs` once you decide on the app's URL prefix.
///
/// `dead_code` is allowed because the matching `.route(...)` line in
/// `urls.rs` ships commented out on purpose — the stub is there to be
/// wired up, so until you do, nothing references it. Without this a
/// freshly generated app warns before you have written a line.
#[allow(dead_code)]
pub async fn hello() -> Html<&'static str> {
    Html(\"<h1>hello from your new app</h1>\")
}
";

/// Default `urls.rs` body: a `pub fn api() -> Router<()>` the
/// project's root `urls.rs` can `.merge(...)`. Admin and tenant
/// dispatch are mounted elsewhere, so this router holds only the
/// app's own routes.
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

/// Default `tests.rs` body, rendered per app so the smoke test can
/// name the starter model's table.
fn render_tests_template(singular_table: &str, crate_root: &str) -> String {
    // `mod.rs` already declares this file as the `tests` module, so
    // the body sits at module scope. Wrapping it in another
    // `mod tests { }` would nest to `<app>::tests::tests` and break
    // the `super::` paths.
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

/// `manage.rs` template for a single-tenant project. It wires up the
/// `rustango::migrate::manage::run` dispatcher. Pass it as
/// [`StartAppOptions::manage_bin`].
pub const SINGLE_TENANT_MANAGE_BIN: &str =
    "//! Generated by `manage startapp --with-manage-bin`. Edit freely.
//!
//! UX: `cargo run -- migrate`,
//! `cargo run -- makemigrations`, etc. The dispatcher is
//! defined in `rustango::migrate::manage`; this binary just hands it
//! the pool and argv.

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

    // `Pool::connect_postgres`, not `PgPool::connect`: the framework's
    // constructor applies the pool options your settings configure.
    let pool = rustango::sql::Pool::connect_postgres(&std::env::var(\"DATABASE_URL\")?).await?;
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

    /// A library's app module must be `pub`, or `src/urls.rs` and the
    /// binaries under `src/bin/` cannot see it.
    #[test]
    fn registering_into_lib_rs_uses_pub_mod() {
        let root = fresh_root("reg_lib");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "//! probe\n\npub mod settings;\npub mod urls;\n",
        )
        .unwrap();

        startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let body = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(
            body.contains("pub mod blog;"),
            "a private `mod blog;` in lib.rs is invisible to src/bin/: {body}"
        );
        // The anchor must be the existing `pub mod` run, not the
        // docstring fallback, which would put it above them.
        assert!(
            body.find("pub mod blog;") > body.find("pub mod settings;"),
            "the new module should join the existing `pub mod` run: {body}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A binary-only project (no lib.rs) keeps the plain `mod`.
    #[test]
    fn registering_into_main_rs_uses_plain_mod() {
        let root = fresh_root("reg_main");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "//! probe\n\nmod urls;\n\nfn main() {}\n",
        )
        .unwrap();

        startapp(
            &root,
            &StartAppOptions {
                app_name: "blog".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let body = std::fs::read_to_string(root.join("src/main.rs")).unwrap();
        assert!(body.contains("mod blog;"), "{body}");
        assert!(
            !body.contains("pub mod blog;"),
            "a binary crate has nothing to export to: {body}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Re-running must not add a second declaration, whichever
    /// spelling is already there.
    #[test]
    fn registering_twice_is_idempotent_across_both_spellings() {
        for (entry, existing) in [
            ("src/lib.rs", "//! probe\n\npub mod blog;\n"),
            ("src/main.rs", "//! probe\n\nmod blog;\n\nfn main() {}\n"),
        ] {
            let root = fresh_root("reg_idem");
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join(entry), existing).unwrap();

            startapp(
                &root,
                &StartAppOptions {
                    app_name: "blog".into(),
                    ..Default::default()
                },
            )
            .unwrap();

            let body = std::fs::read_to_string(root.join(entry)).unwrap();
            assert_eq!(
                body.matches("mod blog;").count(),
                1,
                "{entry} gained a duplicate declaration: {body}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
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
        // Words under 5 characters stay as they are.
        assert_eq!(singularize("dms"), "dms");
        // So do words ending in `ss`, `us`, or not in `s` at all.
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
        // With no override the template must still say `rustango::`.
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
        // A renamed crate root must reach the `use` lines, while the
        // `#[rustango(...)]` attribute name stays as it is: the
        // proc-macro expects that token whatever the crate is called.
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
        // Default root.
        let default_body = render_tests_template("post", "rustango");
        assert!(default_body.contains("use rustango::core::ModelEntry;"));

        // Renamed root.
        let renamed_body = render_tests_template("post", "rustango_orm");
        assert!(renamed_body.contains("use rustango_orm::core::ModelEntry;"));
        assert!(!renamed_body.contains("use rustango::"));
    }

    #[test]
    fn startapp_threads_crate_root_through_models() {
        // End to end: `startapp` must carry `crate_root` all the way
        // into the written `models.rs`, so a renderer refactor
        // cannot quietly drop the override.
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
        // End to end: run `startapp` on a temp project and check the
        // written `models.rs` and `tests.rs` for the singular name,
        // the admin config, and a smoke test on the same table.
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
        // A urls.rs with no router-construction anchor must be left
        // alone, with a manual-step hint instead.
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
