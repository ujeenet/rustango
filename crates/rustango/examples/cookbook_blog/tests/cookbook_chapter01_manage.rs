//! Cookbook Chapter 1 — project shape & manage commands.
//!
//! Each test corresponds 1:1 to a `### N.M` section in COOKBOOK.md.

use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// §1.1 ────────────────────────────────────────────────────────────
#[test]
fn layout_matches_django_shape() {
    let root = project_root();
    for required in [
        "src/main.rs",
        "src/settings.rs",
        "src/apps/mod.rs",
        "config/default.toml",
        // The framework's own tables are no longer shipped as hardcoded
        // bootstrap JSON: they flow through `makemigrations` into a
        // `system/migrations/` folder (generated from the compiled
        // models). The app's own migrations dir carries only user
        // migrations.
        "migrations/0002_cookbook_tables.json",
        "system/migrations",
        "COOKBOOK.md",
        "README.md",
    ] {
        let p = root.join(required);
        assert!(p.exists(), "missing required file: {required}");
    }
    for app in [
        "tenants",
        "auth",
        "blog",
        "media",
        "notify",
        "jobs_demo",
        "search",
        "admin_ui",
    ] {
        let mod_rs = root.join("src/apps").join(app).join("mod.rs");
        assert!(mod_rs.exists(), "missing sub-app mod.rs: {app}");
    }
}

// §1.7 ────────────────────────────────────────────────────────────
//
// Compile-only assertion: `embed_migrations!` expands at build time
// to a `&[(&str, &str)]` containing every JSON file under
// `migrations/`. We re-embed here (the macro is invocation-site;
// each binary that needs it expands its own copy) and assert the
// shape matches what Chapter 1 §1.7 promises.
const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!("migrations");

#[test]
fn embedded_migrations_are_nonempty() {
    assert!(
        !EMBEDDED.is_empty(),
        "embed_migrations!(\"migrations\") returned empty — \
         check that migrations/ contains at least one *.json file",
    );
    for (name, body) in EMBEDDED {
        // The macro strips the `.json` extension — emitted names match
        // the migration's `"name"` field (e.g. `0001_initial`).
        assert!(!name.is_empty(), "name should be non-empty");
        assert!(body.contains("\"name\""), "body missing name field: {name}");
    }
}

// §1.2 / §1.3 / §1.4 / §1.5 / §1.6 ─────────────────────────────────
//
// Live process invocation of the unified `Cli` dispatcher. One test
// covers every verb-recognition recipe because they all flow through
// the same `Cli::run()` argv parser; verb-specific behavior is
// covered by rustango's own integration tests.
#[test]
fn cli_help_works_without_database_url() {
    use std::process::Command;

    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    let out = Command::new(bin)
        .arg("--help")
        .env_remove("DATABASE_URL")
        .output()
        .expect("spawn cookbook_blog --help");

    assert!(
        out.status.success(),
        "--help should exit 0 even without DATABASE_URL; got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "USAGE",
        "create-tenant",
        "migrate",
        "create-operator",
        "create-user",
        "cargo run -- migrate",
    ] {
        assert!(
            stdout.contains(needle),
            "--help output missing `{needle}`. \
             COOKBOOK 1.2/1.3/1.5/1.6 cite this verb. Output:\n{stdout}",
        );
    }
    assert!(
        !stdout.contains("--bin manage"),
        "--help output still references stale `--bin manage`. v0.16 \
         collapsed to one binary; HELP must use `cargo run -- <verb>`. \
         Output:\n{stdout}",
    );
}

// §1.8 ────────────────────────────────────────────────────────────
#[test]
fn settings_layer_resolves_env_overrides() {
    use rustango::config::Settings;

    let root = project_root().join("config");

    let baseline = Settings::load_from(&root, "default").expect("default settings should load");
    let test_overlay = Settings::load_from(&root, "test").expect("test settings should load");

    // config/test.toml overrides pool_max_size = 2 (default is 10).
    assert_eq!(baseline.database.pool_max_size, Some(10));
    assert_eq!(test_overlay.database.pool_max_size, Some(2));

    // RUSTANGO__DATABASE__POOL_MAX_SIZE wins over the file layer.
    // SAFETY: env mutation is single-threaded inside one test process,
    // and we restore at the end. cargo test runs each integration test
    // binary in its own process so cross-test interference is bounded.
    // SAFETY: tests run single-threaded inside one process; we restore.
    unsafe {
        std::env::set_var("RUSTANGO__DATABASE__POOL_MAX_SIZE", "42");
    }
    let envwins = Settings::load_from(&root, "default").expect("env-override load");
    assert_eq!(envwins.database.pool_max_size, Some(42));
    unsafe {
        std::env::remove_var("RUSTANGO__DATABASE__POOL_MAX_SIZE");
    }
}

// §1.9 ────────────────────────────────────────────────────────────
//
// `#[rustango::main]` is what src/main.rs is annotated with, so this
// binary existing at all is the "compiles" half. The "boots" half is
// that the expanded `main` runs far enough to answer `--help`.
#[test]
fn main_macro_compiles_and_boots() {
    let main_rs = std::fs::read_to_string(project_root().join("src/main.rs"))
        .expect("src/main.rs is readable");
    assert!(
        main_rs.contains("#[rustango::main]"),
        "§1.9 documents `#[rustango::main]` as the entry point, but \
         src/main.rs does not use it:\n{main_rs}",
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cookbook_blog"))
        .arg("--help")
        .output()
        .expect("spawn cookbook_blog --help");
    assert!(
        out.status.success(),
        "the `#[rustango::main]` binary should run `--help` successfully; \
         got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
}

// §1.10 ───────────────────────────────────────────────────────────
#[tokio::test]
async fn welcome_page_renders_on_fresh_router() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    let app = rustango::welcome::welcome_router();
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .expect("welcome router answers GET /");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "§1.10 promises a welcome page at GET /",
    );

    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read welcome body");
    let html = String::from_utf8_lossy(&bytes);
    assert!(
        html.contains("rustango"),
        "§1.10's welcome page should name the framework. Got:\n{html}",
    );
}
