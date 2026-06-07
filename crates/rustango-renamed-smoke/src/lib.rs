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

    // Foreign-key model — exercises the `ForeignKey<T>` emission
    // path through the rename. The macro emits per-FK accessor
    // methods (`.author_get_pool(...)` / `.author_set(...)`) plus
    // `LoadRelated::__rustango_load_related` for `select_related`
    // hydration. If the renamed-crate path resolution broke any of
    // these emit sites, this struct wouldn't compile.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "rrs_demo_child")]
    pub struct RenamedDemoChild {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 80)]
        pub label: String,
        pub parent: orm::sql::ForeignKey<RenamedDemo>,
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

    // ViewSet covers the `derive_viewset` path-resolution branch —
    // the macro emits `pub fn router(...) -> #root::__axum::Router`
    // (the `__axum` re-export was added as a #142 follow-up so this
    // emit no longer needs a hardcoded `::axum::` path). ViewSet
    // derive is gated on `tenancy` in rustango; axum on `admin`.
    // Both features are enabled by this crate's defaults.
    #[cfg(all(feature = "admin", feature = "tenancy"))]
    #[derive(orm::ViewSet)]
    #[viewset(
        model         = RenamedDemo,
        fields        = "id, name, views",
        page_size     = 20,
    )]
    #[allow(dead_code)]
    pub struct RenamedDemoViewSet;

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

        #[test]
        fn fk_model_schema_resolves_through_renamed_dep() {
            // The FK-bearing model exercises a different macro
            // emit path (`load_related_impl_tokens`, the per-FK
            // accessor emission, and the relation registration in
            // ModelSchema). If any of those used hardcoded
            // ::rustango:: paths, this struct wouldn't compile.
            assert_eq!(RenamedDemoChild::SCHEMA.table, "rrs_demo_child");
            // Confirm the FK relation registered correctly — the
            // `parent` field carries a `Relation::Fk` because the
            // macro saw `ForeignKey<RenamedDemo>` at derive time.
            let parent_field = RenamedDemoChild::SCHEMA
                .field("parent")
                .expect("parent field exists");
            assert!(
                matches!(parent_field.relation, Some(orm::core::Relation::Fk { .. })),
                "expected parent field's relation to be Fk, got {:?}",
                parent_field.relation
            );
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

        #[cfg(all(feature = "admin", feature = "tenancy"))]
        #[test]
        fn viewset_router_method_resolves_through_renamed_dep() {
            // ViewSet derive emits a `router(prefix, pool) ->
            // #root::__axum::Router` method. We can't actually
            // build the router without a real PgPool, but we can
            // confirm the method's type signature compiles + the
            // axum::Router return type resolves through the
            // renamed crate. Reaching this assertion at all is
            // the test — compilation proves the rename worked.
            let _router_fn: fn(&str, orm::sql::sqlx::PgPool) -> orm::__axum::Router =
                RenamedDemoViewSet::router;
        }

        #[test]
        fn embed_migrations_macro_resolves_through_renamed_dep() {
            // embed_migrations!() is a proc-macro that walks a
            // directory at compile time and emits a slice of
            // (name, content) pairs. Path resolution flows through
            // `expand_embed_migrations` (the macro entry point has
            // no `::rustango::` sites itself — confirmed in #142
            // Phase 2 audit — so this test mainly proves the entry
            // point's signature works under the rename).
            const EMBEDDED: &[(&str, &str)] = orm::embed_migrations!("./migrations");
            assert_eq!(EMBEDDED.len(), 1);
            assert_eq!(EMBEDDED[0].0, "0001_initial");
            assert!(EMBEDDED[0].1.contains("\"forward\""));
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
