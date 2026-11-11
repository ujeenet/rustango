//! `#[derive(Model)]` output must build under `#![deny(missing_docs)]`.
#![deny(missing_docs)]

/// Models of an app that denies undocumented public items.
pub mod models {
    use rustango::sql::{Auto, ForeignKey};
    use rustango::Model;

    /// An author.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "md_author")]
    pub struct Author {
        /// Primary key.
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        /// Display name.
        #[rustango(max_length = 80)]
        pub name: String,
    }

    /// A post.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "md_post")]
    pub struct Post {
        /// Primary key.
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        /// Who wrote it.
        pub author: ForeignKey<Author>,
        /// Title.
        #[rustango(max_length = 120)]
        pub title: String,
    }
}

#[test]
fn derived_items_carry_docs() {
    use rustango::core::Model as _;
    assert_eq!(models::Author::SCHEMA.table, "md_author");
    assert_eq!(models::Post::SCHEMA.table, "md_post");
}
