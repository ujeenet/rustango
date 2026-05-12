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
// v0.38 — `server::Builder` is the multi-tenant Postgres runserver
// (`TenantPools`, schema-mode dispatch, operator console). The Pool
// enum lift lands in slice 5; for now sqlite/mysql apps mount their
// own routes against `DatabasePools<DB>` + plain `axum::serve`.
#[cfg(all(feature = "tenancy", feature = "postgres"))]
mod builder;

#[cfg(feature = "runserver")]
pub use app::AppBuilder;
#[cfg(all(feature = "tenancy", feature = "postgres"))]
pub use builder::{ApiRouter, Builder};
