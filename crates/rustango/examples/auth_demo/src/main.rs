//! `auth_demo` entrypoint — the unified manage CLI (`cargo run -- migrate`,
//! `makemigrations`, `runserver`, …). The authentication flows themselves are
//! exercised by the integration tests in `tests/`; this binary just provides
//! the migrate/runserver harness the docs' "Runnable version" commands point at.

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    // Link the lib's model inventory into the binary so `migrate` /
    // `makemigrations` see the `auth_users` table.
    let _ = std::any::type_name::<auth_demo::User>();
    rustango::manage::Cli::new().with_health().run().await
}
