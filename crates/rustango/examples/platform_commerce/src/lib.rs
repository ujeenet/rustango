//! The commerce domain, as a library.
//!
//! The library target exists because `src/bin/commerce_worker.rs` is
//! **its own crate**, not a module of `main.rs` — a binary under
//! `src/bin/` cannot see `mod commerce;` declared in another binary. So
//! anything both the server and the worker need has to live here.
//!
//! This matters more than it looks: the worker's whole job is to
//! register the same job types the server dispatches. A worker that
//! registers a different set — or none — picks up rows it has no
//! handler for and returns *without unlocking them*, stranding them
//! where `pending_count()` cannot see them. Sharing one module is what
//! makes "the same four types" structurally true rather than a comment.

pub mod commerce;
