//! Binary shim. Wires the `AppUser` override into the unified `Cli`
//! dispatcher; everything else delegates to the lib crate.

use tenant_user_extension::{api, models::AppUser};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    rustango::manage::Cli::new()
        .tenancy()
        .user_model::<AppUser>()
        .api(api::router())
        .migrations_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
        .run()
        .await
}
