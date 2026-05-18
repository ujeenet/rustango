//! Four models — two polymorphic targets (`Post`, `Article`) and two
//! polymorphic children (`Tag`, `Comment`). Both children carry a
//! `generic_fk` so a single row can attach to either target type.

use rustango::register_admin_inline_generic;
use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "gfkdemo_post",
    app = "gfkdemo",
    display = "title",
    admin(
        list_display = "title, published_at",
        search_fields = "title",
        ordering = "-published_at",
    )
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(
        max_length = 200,
        help_text = "Short, descriptive headline shown in listings and feeds."
    )]
    pub title: String,
    #[rustango(
        max_length = 8000,
        help_text = "Markdown is supported. First paragraph appears as the summary on index pages."
    )]
    pub body: String,
    #[rustango(help_text = "Future timestamps schedule the post; leave blank to publish now.")]
    pub published_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "gfkdemo_article",
    app = "gfkdemo",
    display = "title",
    admin(
        list_display = "title, published_at",
        search_fields = "title",
        ordering = "-published_at",
    )
)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 8000)]
    pub body: String,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

/// Tag — attaches to either a Post or an Article (or anything else
/// you register). The `generic_fk` declares the polymorphic pair:
/// `(content_type_id, object_pk)` together identify the target row.
///
/// `list_display = "name, target"` collapses the two FK columns into
/// a single clickable target-link cell (#241).
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "gfkdemo_tag",
    app = "gfkdemo",
    display = "name",
    generic_fk(
        name = "target",
        ct_column = "content_type_id",
        pk_column = "object_pk"
    ),
    admin(list_display = "name, target", search_fields = "name")
)]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
    pub object_pk: i64,
    #[rustango(
        max_length = 40,
        help_text = "Lowercase, no spaces. Used in URLs and filter chips."
    )]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "gfkdemo_comment",
    app = "gfkdemo",
    generic_fk(
        name = "target",
        ct_column = "content_type_id",
        pk_column = "object_pk"
    ),
    admin(list_display = "body, target")
)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub content_type_id: i64,
    pub object_pk: i64,
    #[rustango(max_length = 500)]
    pub body: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// Register the same Tag + Comment as generic inlines under BOTH Post
// and Article. One declaration per (parent_table, child_table) pair.
register_admin_inline_generic!(
    parent = "gfkdemo_post",
    child = "gfkdemo_tag",
    ct = "content_type_id",
    pk = "object_pk",
    kind = rustango::admin::InlineKind::Tabular,
    label = "Tags",
    fields = &["name"],
    extra = 1,
);

register_admin_inline_generic!(
    parent = "gfkdemo_post",
    child = "gfkdemo_comment",
    ct = "content_type_id",
    pk = "object_pk",
    kind = rustango::admin::InlineKind::Stacked,
    label = "Comments",
    fields = &["body", "created_at"],
    extra = 1,
);

register_admin_inline_generic!(
    parent = "gfkdemo_article",
    child = "gfkdemo_tag",
    ct = "content_type_id",
    pk = "object_pk",
    kind = rustango::admin::InlineKind::Tabular,
    label = "Tags",
    fields = &["name"],
    extra = 1,
);

register_admin_inline_generic!(
    parent = "gfkdemo_article",
    child = "gfkdemo_comment",
    ct = "content_type_id",
    pk = "object_pk",
    kind = rustango::admin::InlineKind::Stacked,
    label = "Comments",
    fields = &["body", "created_at"],
    extra = 1,
);
