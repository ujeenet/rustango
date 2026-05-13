//! `rustango::server` — Django-style runserver builder.
//!
//! Owns every line of boilerplate that's identical across tenancy
//! apps: connect to `DATABASE_URL`, build `TenantPools`, mount the
//! resolver chain, host-dispatch apex → operator console / subdomain
//! → tenant admin + user routes, bind + serve.
//!
//! ```ignore
//! use rustango::server::Builder;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     tracing_subscriber::fmt().init();
//!     Builder::from_env().await?
//!         .admin_show_only(["author", "post"])
//!         .api(my_app::urls::api())
//!         .seed_with(|pools, registry, registry_url| async move {
//!             my_app::seed::run(&pools, &registry, &registry_url).await
//!         })
//!         .await?
//!         .serve("0.0.0.0:8080").await
//! }
//! ```

#[cfg(feature = "runserver")]
mod app;
// `server::Builder` is generic over `DB: sqlx::Database` — the
// PG-specific bits (`from_env`, `PgPool::connect`) live behind their
// own `#[cfg(feature = "postgres")]` inside `mod builder`. The rest
// of the assembly (`from_pool`, `migrate`, `serve`) speaks the
// erased `rustango::sql::Pool`, so sqlite + mysql tenancy apps can
// reach it without an extra adapter.
#[cfg(feature = "tenancy")]
mod builder;

#[cfg(feature = "runserver")]
pub use app::AppBuilder;
#[cfg(feature = "tenancy")]
pub use builder::{ApiRouter, Builder};
