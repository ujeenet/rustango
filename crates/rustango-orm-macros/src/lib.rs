//! Proc-macros for the standalone `rustango-orm` crate — carve-out
//! of [`rustango-macros`](https://docs.rs/rustango-macros).
//!
//! This crate re-exports the ORM-only proc-macros from the
//! workspace's `rustango-macros` crate:
//!
//! - [`Model`] — `#[derive(Model)]` populates the model schema,
//!   generates inherent methods (insert / save / delete / find /
//!   where_ / sum / etc.), and registers the model with the
//!   `inventory` crate so the admin walks every derived model.
//! - [`Form`] — `#[derive(Form)]` validates user input and parses
//!   it into a strongly-typed struct.
//! - [`embed_migrations!`] — bakes a `migrations/` directory's
//!   `.json` files into a `&'static [(name, content)]` slice at
//!   compile time.
//!
//! The framework-only derives (`Serializer`, `ViewSet`, `Q!`,
//! `#[rustango::main]`) stay in `rustango-macros`. Downstream code
//! that depends only on `rustango-orm-macros` literally cannot
//! reach them — that's the carve-out half of issue
//! [#143](https://github.com/ujeenet/rustango/issues/143).
//!
//! ## How re-export works for proc-macros
//!
//! Rust's resolver follows `pub use` paths to find a proc-macro's
//! defining crate. The consumer writes:
//!
//! ```ignore
//! use rustango_orm_macros::Model;
//!
//! #[derive(Model)]
//! pub struct Post { … }
//! ```
//!
//! …and the compiler looks up `Model` in this crate's scope, sees
//! it's a re-export of `rustango_macros::Model`, walks back to the
//! `proc-macro = true` crate where the derive is actually defined,
//! and invokes the proc-macro entry point.
//!
//! Because of how proc-macros work, the re-exporter itself doesn't
//! need `proc-macro = true`. This is a regular `[lib]` crate.
//!
//! After issue [#144](https://github.com/ujeenet/rustango/issues/144)
//! lands (the physical move of the ORM modules to a standalone
//! `rustango-orm` crate), the proc-macro bodies themselves will
//! migrate here and `rustango-macros` will drop them, completing
//! the carve-out.

#![warn(missing_docs)]

pub use rustango_macros::{embed_migrations, Form, Model};
