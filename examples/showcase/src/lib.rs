//! rustango-showcase library — exposes apps + the embedded migrations
//! const so the bin shim and E2E playwright suite share one source of
//! truth.
//!
//! Phase 1 (this commit): scaffold + smoke endpoint via `manage`
//! runserver. Per-app routers (`blog`, `shop`, `accounts`, etc.)
//! plug in under [`apps::api`] as subsequent phases land.

pub mod apps;

/// Embedded migrations from `migrations/`. Compile-time validated:
/// `embed_migrations!` panics at build time when the directory
/// references break (missing predecessor, malformed JSON, etc.).
pub const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!("migrations");
