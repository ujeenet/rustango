//! Integration test for slice 9.0g — verify the macro + inventory
//! pipeline records `module_path!()` correctly and
//! `ModelEntry::resolved_app_label` infers a Django-shape app label
//! from a model's module location, with the explicit
//! `#[rustango(app = "...")]` attribute taking precedence.

use rustango::core::{inventory, ModelEntry};
use rustango::sql::Auto;
use rustango::Model;

mod blog {
    pub mod models {
        use super::super::*;
        #[derive(Model, Debug, Clone)]
        #[rustango(table = "app_label_blog_post")]
        #[allow(dead_code)]
        pub struct Post {
            #[rustango(primary_key)]
            pub id: Auto<i64>,
            #[rustango(max_length = 200)]
            pub title: String,
        }
    }
}

mod shop {
    pub mod models {
        use super::super::*;
        #[derive(Model, Debug, Clone)]
        // Explicit override — shouldn't matter that this lives in
        // `crate::shop::models`; the override forces `app_label =
        // "marketplace"`.
        #[rustango(table = "app_label_shop_item", app = "marketplace")]
        #[allow(dead_code)]
        pub struct Item {
            #[rustango(primary_key)]
            pub id: Auto<i64>,
            #[rustango(max_length = 80)]
            pub name: String,
        }
    }
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "app_label_root_model")]
#[allow(dead_code)]
pub struct RootModel {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

fn entry_for(table: &str) -> Option<&'static ModelEntry> {
    inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == table)
}

#[test]
fn module_path_inference_picks_blog() {
    let entry = entry_for("app_label_blog_post").expect("Post is registered");
    assert!(
        entry.module_path.ends_with("blog::models"),
        "module_path was {:?}",
        entry.module_path
    );
    assert_eq!(entry.resolved_app_label(), Some("blog"));
    // No explicit override on the schema itself.
    assert_eq!(entry.schema.app_label, None);
}

#[test]
fn explicit_attr_wins_over_module_path() {
    let entry = entry_for("app_label_shop_item").expect("Item is registered");
    assert_eq!(entry.schema.app_label, Some("marketplace"));
    assert_eq!(entry.resolved_app_label(), Some("marketplace"));
}

#[test]
fn root_level_model_has_no_app_label() {
    let entry = entry_for("app_label_root_model").expect("RootModel is registered");
    // module_path looks like "app_label_inference::RootModel" — first
    // segment is the test binary, no app folder, returns None.
    assert_eq!(entry.resolved_app_label(), None);
}
