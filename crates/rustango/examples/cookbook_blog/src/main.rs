//! cookbook_blog binary shim. All logic lives in
//! [`cookbook_blog::apps`] / [`cookbook_blog::settings`] so
//! integration tests can import them.

use cookbook_blog::{apps, settings};

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
