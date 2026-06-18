//! `auth_demo` — companion crate for the rustango Authentication deep-dive docs
//! (`docs/auth-*.md`).
//!
//! The library holds the shared app-level models; each authentication flow is
//! exercised by an integration test under `tests/auth_<flow>.rs`, which is the
//! runnable, CI-tested source the matching doc copies its snippets from.

pub mod models;

pub use models::User;
