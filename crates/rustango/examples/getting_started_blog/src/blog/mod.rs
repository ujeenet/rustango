//! `blog` — Django-shape app module.
//!
//! Add `mod blog;` (or `pub mod blog;`) to your
//! `src/main.rs` / `src/lib.rs` so these submodules are
//! pulled into the binary's `inventory` registry.

pub mod models;
pub mod urls;
pub mod views;

#[cfg(test)]
mod tests;
