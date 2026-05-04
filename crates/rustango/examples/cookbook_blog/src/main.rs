//! cookbook_blog — multi-tenant blog example. One binary handles
//! `cargo run` (runserver) AND `cargo run -- <verb>` via the unified
//! `rustango::manage::Cli` (v0.16). See [`crate::apps`] for the
//! per-feature recipes the COOKBOOK chapters cite.

mod apps;
mod settings;

/// Embedded migrations — Chapter 1 §1.7. Surfaces as a `&[(&str, &str)]`
/// of `(file_name, json_body)` tuples. Compile-time validated against
/// `migrations/`.
pub const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!("migrations");

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let _settings = settings::load()?;

    rustango::manage::Cli::new()
        .tenancy()
        .api(apps::api())
        .migrations_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
        .run()
        .await
}
