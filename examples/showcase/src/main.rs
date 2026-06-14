//! rustango-showcase binary shim. All logic lives in
//! [`rustango_showcase::apps`] so the E2E playwright suite (and
//! future integration tests) can import them.
//!
//! Entry point is the framework's `manage::Cli`: same verb surface
//! as a normal rustango project (`migrate`, `makemigrations`,
//! `runserver`, etc.). The E2E job invokes `cargo run -- migrate
//! --apply-all` before `cargo run -- runserver` so the DB matches
//! the embedded migration set before playwright hits any URL.

use rustango_showcase::apps;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    rustango::manage::Cli::new()
        .api(apps::api())
        .migrations_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
        .run()
        .await
}
