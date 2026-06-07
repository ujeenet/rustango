//! Smoke test for [#142](https://github.com/ujeenet/rustango/issues/142).
//!
//! The Cargo.toml renames the rustango dep to `orm` via `package =
//! "rustango"`. If `#[derive(Model)]` emitted hardcoded
//! `::rustango::...` paths (the pre-#142 behavior), this crate
//! would fail to compile here — `::rustango::core::Model` would
//! resolve to nothing because the consumer's Cargo.toml has no
//! `rustango` dep entry.
//!
//! After the four-phase migration that closed #142 (PRs #898–#901),
//! every macro emit routes through `rustango_root()` which reads
//! the consumer's manifest at expansion time and returns the local
//! name (`orm` here). The macro emits `::orm::core::Model`, the
//! type resolves, and `cargo build --package rustango-renamed-smoke`
//! passes.
//!
//! The crate doesn't need to do anything at runtime — just compile.
//! If it builds, the test passes.
//!
//! The body is gated on this crate's own `postgres` feature for
//! two reasons:
//!
//! 1. The macro emits `#[cfg(feature = "postgres")] impl LoadRelated
//!    for #StructName` etc. against the CONSUMER's feature flags.
//!    Without the postgres feature on the consumer side, those impls
//!    get silently dropped and the trait bounds elsewhere go
//!    unsatisfied.
//! 2. The litmus `cargo build --no-default-features --features
//!    sqlite,tenancy` run from the workspace root must keep passing.
//!    Under those flags this crate's `postgres` feature is OFF, so
//!    the body compiles to nothing and the smoke crate is
//!    effectively absent from the litmus build.

#![allow(dead_code)]

#[cfg(feature = "postgres")]
mod gated {
    use orm::sql::Auto;
    use orm::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "rrs_demo")]
    pub struct RenamedDemo {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 80)]
        pub name: String,
        pub views: i64,
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use orm::core::Model as _;

        #[test]
        fn schema_resolves_through_renamed_dep() {
            // If the macro's path resolution broke, this wouldn't
            // even compile. Reaching this assertion at all is the
            // test.
            assert_eq!(RenamedDemo::SCHEMA.table, "rrs_demo");
            assert!(RenamedDemo::SCHEMA.primary_key().is_some());
        }
    }
}
