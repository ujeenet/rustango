//! Demo: extra columns on `rustango_users` via the
//! [`rustango::tenancy::TenantUserModel`] trait.
//!
//! See `README.md` for the runnable walkthrough. The library exposes
//! [`models::AppUser`] (the framework's `User` shape + two extra
//! columns) and [`api::router`] (a tiny JSON API showcasing how to
//! read those extras).

pub mod api;
pub mod models;
