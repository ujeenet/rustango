//! `#[rustango(verbose_name = "...", verbose_name_plural = "...")]` on a
//! model struct (Django parity #320 — `Meta.verbose_name` /
//! `Meta.verbose_name_plural`).
//!
//! Covers:
//! - macro threads both values onto `ModelSchema::verbose_name` /
//!   `verbose_name_plural`
//! - `display_label()` returns `verbose_name` when set, else struct name
//! - `display_label_plural()` prefers `verbose_name_plural`, else
//!   `verbose_name + 's'`, else `name + 's'`
//! - explicit plural overrides the auto-`s` suffix
//! - DDL stays unchanged (presentation-only attribute)

use rustango::core::Model;
use rustango::migrate::ddl::create_table_sql_with_dialect;
use rustango::sql::Postgres;
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_meta_vn_post")]
#[rustango(verbose_name = "blog post")]
#[rustango(verbose_name_plural = "blog posts")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
}

/// Model with only singular `verbose_name` set — plural form should
/// auto-suffix `s`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_meta_vn_singular_only")]
#[rustango(verbose_name = "tag")]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 50)]
    pub name: String,
}

/// Model with no verbose_name at all — both labels fall back to the
/// struct identifier.
#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_meta_vn_unset")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn schema_threads_both_verbose_names_when_set() {
    assert_eq!(Post::SCHEMA.verbose_name, Some("blog post"));
    assert_eq!(Post::SCHEMA.verbose_name_plural, Some("blog posts"));
}

#[test]
fn singular_only_threads_just_verbose_name() {
    assert_eq!(Tag::SCHEMA.verbose_name, Some("tag"));
    assert!(Tag::SCHEMA.verbose_name_plural.is_none());
}

#[test]
fn unset_falls_through_to_none() {
    assert!(Comment::SCHEMA.verbose_name.is_none());
    assert!(Comment::SCHEMA.verbose_name_plural.is_none());
}

#[test]
fn display_label_prefers_verbose_name_then_struct_name() {
    assert_eq!(Post::SCHEMA.display_label(), "blog post");
    assert_eq!(Tag::SCHEMA.display_label(), "tag");
    assert_eq!(Comment::SCHEMA.display_label(), "Comment");
}

#[test]
fn display_label_plural_prefers_explicit_then_suffix_then_struct_name() {
    // Explicit plural wins
    assert_eq!(Post::SCHEMA.display_label_plural(), "blog posts");
    // No explicit plural → verbose_name + "s"
    assert_eq!(Tag::SCHEMA.display_label_plural(), "tags");
    // No verbose_name either → struct name + "s"
    assert_eq!(Comment::SCHEMA.display_label_plural(), "Comments");
}

/// Belt-and-braces: `verbose_name` is presentation-only and must not
/// leak into the CREATE TABLE output. Table identifier stays as the
/// declared SQL name.
#[test]
fn verbose_name_does_not_change_ddl() {
    let sql = create_table_sql_with_dialect(&Postgres, Post::SCHEMA);
    assert!(
        !sql.contains("blog post"),
        "verbose_name leaked into DDL: {sql}"
    );
    assert!(sql.contains(r#""macro_meta_vn_post""#));
}
