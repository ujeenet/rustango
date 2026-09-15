//! `commerce` — the domain, multi-tenant.
//!
//! Scaffolded by `manage startapp commerce`, then filled in. The twin
//! of this module lives in `platform_commerce`. `models.rs`,
//! `serializers.rs`, `jobs.rs`, `views.rs` and `tests.rs` are byte for
//! byte identical there; `urls.rs` differs only in swapping
//! `router_pool` for `tenant_router`, and `supervisor.rs` has no
//! single-tenant counterpart because one pool needs no supervisor.

pub mod jobs;
pub mod models;
pub mod serializers;
pub mod supervisor;
pub mod urls;
pub mod views;

#[cfg(test)]
mod tests;
