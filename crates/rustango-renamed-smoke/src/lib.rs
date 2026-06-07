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

    // Serializer covers the `derive_serializer` path-resolution
    // branch — the macro emits `#root::serializer::ModelSerializer`
    // impls plus a tuple-positional view. If the rename broke the
    // serializer-side emit, this struct wouldn't compile.
    #[cfg(feature = "serializer")]
    #[derive(orm::Serializer, Default)]
    #[serializer(model = RenamedDemo)]
    pub struct RenamedDemoSerializer {
        pub name: String,
        pub views: i64,
    }

    // Form covers the `derive_form` path-resolution branch —
    // the macro emits `#root::forms::Form` impls + per-field
    // validator chains. If the rename broke the form-side emit,
    // this struct wouldn't compile.
    #[cfg(feature = "forms")]
    #[derive(orm::Form, Debug)]
    pub struct RenamedDemoForm {
        #[form(min_length = 1, max_length = 80)]
        pub name: String,
        #[form(min = 0)]
        pub views: i32,
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

        #[cfg(feature = "serializer")]
        #[test]
        fn serializer_resolves_through_renamed_dep() {
            use orm::serializer::ModelSerializer;
            let demo = RenamedDemo {
                id: Auto::Set(7),
                name: "ada".into(),
                views: 42,
                created_at: chrono::Utc::now(),
            };
            let s = RenamedDemoSerializer::from_model(&demo);
            assert_eq!(s.name, "ada");
            assert_eq!(s.views, 42);
            let json = s.to_value();
            assert_eq!(json["name"], "ada");
            assert_eq!(json["views"], 42);
        }

        #[cfg(feature = "forms")]
        #[test]
        fn form_resolves_through_renamed_dep() {
            use orm::forms::Form;
            let mut payload = ::std::collections::HashMap::new();
            payload.insert("name".to_string(), "ada".to_string());
            payload.insert("views".to_string(), "42".to_string());
            let f = RenamedDemoForm::parse(&payload).expect("valid payload");
            assert_eq!(f.name, "ada");
            assert_eq!(f.views, 42);
        }

        #[test]
        fn q_macro_resolves_through_renamed_dep() {
            // Q!() is a proc-macro that emits `Column::like(...)` /
            // `eq(...)` / etc. against the model's typed column
            // surface. Path resolution flows through `expand_q` ->
            // `#root::core::Column::...` (migrated in #899).
            // If the rename broke that path, this wouldn't compile.
            //
            // Q!() returns `TypedFilter<Model>`; call `.into_expr()`
            // to get the dialect-neutral `WhereExpr`.
            let q = orm::Q!(RenamedDemo.name__icontains = "ada");
            let where_expr: orm::core::WhereExpr = q.into();
            // The exact IR shape is an implementation detail; just
            // assert we got a non-empty leaf — the rename is what
            // we're testing, not the predicate writer.
            assert!(!matches!(
                where_expr,
                orm::core::WhereExpr::And(ref v) if v.is_empty()
            ));
        }
    }
}
