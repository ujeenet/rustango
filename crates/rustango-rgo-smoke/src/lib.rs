//! Builds only if `#[derive(Model)]` resolves its paths through a
//! dependency renamed `rgo` and reads `#[rgo(...)]` like `#[rustango(...)]`.
//!
//! The body is gated on this crate's `postgres` feature for the reason
//! `rustango-renamed-smoke` gives: the derive's impls follow the
//! consumer's features, and the sqlite litmus build turns it off.

#![allow(dead_code)]

#[cfg(feature = "postgres")]
mod gated {
    use rgo::prelude::*;

    #[derive(Model, Debug, Clone)]
    #[rgo(table = "rgo_smoke_author", display = "name")]
    pub struct Author {
        #[rgo(primary_key)]
        pub id: Auto<i64>,
        #[rgo(max_length = 80)]
        pub name: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rgo(table = "rgo_smoke_post")]
    pub struct Post {
        #[rgo(primary_key)]
        pub id: Auto<i64>,
        #[rgo(max_length = 120)]
        pub title: String,
        pub author: rgo::sql::ForeignKey<Author>,
    }

    #[cfg(feature = "serializer")]
    #[derive(rgo::Serializer, Default)]
    #[serializer(model = Author)]
    pub struct AuthorSerializer {
        pub name: String,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rgo_attributes_reach_the_schema() {
            assert_eq!(Author::SCHEMA.table, "rgo_smoke_author");
            assert_eq!(Author::SCHEMA.field("name").unwrap().max_length, Some(80));
            assert!(matches!(
                Post::SCHEMA.field("author").unwrap().relation,
                Some(rgo::core::Relation::Fk { .. })
            ));
        }

        #[test]
        fn q_macro_resolves_through_rgo() {
            let q = rgo::Q!(Author.name__icontains = "ada");
            let _: rgo::core::WhereExpr = q.into();
        }

        #[cfg(feature = "serializer")]
        #[test]
        fn serializer_resolves_through_rgo() {
            use rgo::serializer::ModelSerializer;
            let a = Author {
                id: Auto::Set(1),
                name: "ada".into(),
            };
            assert_eq!(AuthorSerializer::from_model(&a).to_value()["name"], "ada");
        }
    }
}
