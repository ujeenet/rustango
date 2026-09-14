//! `commerce` — the domain.
//!
//! Scaffolded by `manage startapp commerce`, then filled in. The twin
//! of this module lives in `platform_commerce_saas`; see the header of
//! each file for what differs (almost nothing — `urls.rs` swaps
//! `router_pool` for `tenant_router`, and that is the point).

pub mod jobs;
pub mod models;
pub mod serializers;
pub mod urls;
pub mod views;

#[cfg(test)]
mod tests;
