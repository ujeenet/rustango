//! Project entrypoint — `Cli::run()` is the unified dispatcher
//! that handles `cargo run` (runserver) AND `cargo run -- migrate` /
//! `makemigrations` / `startapp` / etc. from one binary. No
//! `src/bin/manage.rs` needed. The auto-admin is at `/admin` (or
//! `[admin] url_prefix`), behind a superuser login; make the account with
//! `cargo run -- create-admin <username> --superuser`.
//!
//! Logging is auto-configured by `#[rustango::main]` —
//! `tracing_subscriber::fmt` with env-filter, default
//! `info,sqlx=warn`. Override with `RUST_LOG` (env, or `./.env`: the macro reads that key)
//! or replace the macro with a hand-rolled subscriber in front of
//! `Cli::new()` for JSON / file-rotation / OTel export.

// See the note in the api template: the app's modules live in the
// library target so `src/bin/*.rs` binaries can reach them.
use getting_started_blog::urls;

// Errors print with Display, so a multi-line CLI error reads as lines.
#[rustango::main]
async fn main() -> std::process::ExitCode {
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    // config/*.toml for the RUSTANGO_ENV tier (default `dev`), then
    // RUSTANGO__* env overrides; loaded once for the server and the admin.
    // No config/ dir: a warning, and both run on defaults.
    let loaded = rustango::config::Settings::load_from_env();
    let settings = loaded.as_ref().cloned().unwrap_or_default();
    let admin_settings = settings.clone();
    rustango::manage::Cli::new()
        .api(urls::api())
        // Built from the server's own pool, so other verbs need no database.
        .nest_with(urls::admin_prefix(&settings), move |pool| {
            urls::admin_router(pool, &admin_settings)
        })
        // Step 12b: the ViewSet, on the server's pool like the admin.
        .nest_with("/api", |pool| {
            getting_started_blog::post_view_set::PostViewSet::router("/posts", pool)
        })
        .with_health() // /health + /ready endpoints for load balancers
        // Behind a load balancer, name its addresses so per-IP limits see clients:
        // .with_trusted_proxies(["10.0.0.0/8"])?
        .with_loaded_settings(loaded)
        .run()
        .await
}
