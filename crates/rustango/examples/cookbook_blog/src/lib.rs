//! cookbook_blog library — exposes apps + settings + the embedded
//! migrations const so the bin shim and integration tests share one
//! source of truth.

pub mod apps;
pub mod settings;

/// Embedded migrations — Chapter 1 §1.7. Surfaces as a `&[(&str, &str)]`
/// of `(file_name, json_body)` tuples. Compile-time validated against
/// `migrations/`.
pub const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!("migrations");
