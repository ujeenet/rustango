//! Library target — re-exports the modules so integration tests in
//! `tests/` (a separate crate) can `use getting_started_blog::…`.
//! The matching `mod …;` lines stay in `src/main.rs` for the binary.

pub mod blog;
pub mod models;
pub mod post_serializer;
pub mod post_view_set;
pub mod urls;
pub mod views;
