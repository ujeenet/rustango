//! Tri-dialect emission tests for the **M2M** and **generic-FK (GFK)**
//! arms of the relation-existence / eager-aggregate family — issue #830.
//!
//! `where_has` / `where_doesnt_have` / `where_has_count` and
//! `annotate_count` / `annotate_sum` / `annotate_exists` resolve a
//! relation by name across reverse-FK **and** M2M (junction table) **and**
//! GFK (content-type-discriminated child). These tests pin the generated
//! SQL — the raw-table `EXISTS`/aggregate that the junction (no
//! `ModelSchema`) and the polymorphic child require.

use rustango::core::{Model as _, Op};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

// ---- M2M: Post <-> Tag through rmg_post_tags --------------------------

#[derive(Model)]
#[rustango(
    table = "rmg_post",
    m2m(
        name = "tags",
        to = "rmg_tag",
        through = "rmg_post_tags",
        src = "post_id",
        dst = "tag_id",
        auto_create = false,
    )
)]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
}

#[derive(Model)]
#[rustango(table = "rmg_tag")]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 40)]
    name: String,
    weight: i64,
}

// ---- GFK: Article with generic comments (GComment points back) --------

#[derive(Model)]
#[rustango(
    table = "rmg_article",
    generic_has(
        name = "comments",
        child = "GComment",
        ct_column = "content_type_id",
        pk_column = "object_pk"
    )
)]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    title: String,
}

#[derive(Model)]
#[rustango(table = "rmg_gcomment")]
#[allow(dead_code)]
pub struct GComment {
    #[rustango(primary_key)]
    id: i64,
    content_type_id: i64,
    object_pk: i64,
    #[rustango(max_length = 200)]
    body: String,
    score: i64,
}

// ----------------------------------------------------------------- M2M

#[test]
fn m2m_where_has_emits_exists_over_junction() {
    let q = Post::objects()
        .where_has("tags")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_select(&q).expect("emit").sql;
    assert!(
        sql.contains(
            r#"EXISTS (SELECT 1 FROM "rmg_post_tags" WHERE "rmg_post_tags"."post_id" = "rmg_post"."id")"#
        ),
        "M2M where_has should EXISTS over the junction: {sql}"
    );
}

#[test]
fn m2m_where_doesnt_have_emits_not_exists() {
    let q = Post::objects()
        .where_doesnt_have("tags")
        .compile()
        .expect("compile");
    let sql = Sqlite.compile_select(&q).expect("emit").sql;
    assert!(
        sql.contains(r#"NOT EXISTS (SELECT 1 FROM "rmg_post_tags" WHERE "rmg_post_tags"."post_id" = "rmg_post"."id")"#),
        "M2M where_doesnt_have should NOT EXISTS over the junction: {sql}"
    );
}

#[test]
fn m2m_where_has_count_emits_correlated_junction_count() {
    let q = Post::objects()
        .where_has_count("tags", Op::Gt, 2)
        .compile()
        .expect("compile");
    let sql = Postgres.compile_select(&q).expect("emit").sql;
    assert!(
        sql.contains(
            r#"(SELECT COUNT(*) FROM "rmg_post_tags" WHERE "rmg_post_tags"."post_id" = "rmg_post"."id") > $1"#
        ),
        "M2M has(count) should compare a correlated junction COUNT: {sql}"
    );
}

#[test]
fn m2m_annotate_count_projects_junction_count() {
    let q = Post::objects()
        .annotate_count("tags")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit").sql;
    assert!(
        sql.contains(
            r#"(SELECT COUNT(*) FROM "rmg_post_tags" WHERE "rmg_post_tags"."post_id" = "rmg_post"."id") AS "tags_count""#
        ),
        "M2M annotate_count column: {sql}"
    );
    assert!(
        sql.contains(r#"GROUP BY "id", "title""#),
        "Shape-3 GROUP BY over parent columns: {sql}"
    );
}

#[test]
fn m2m_annotate_sum_aggregates_target_column_through_junction() {
    let q = Post::objects()
        .annotate_sum("tags", "weight")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit").sql;
    // SUM over the *target* table, members selected via the junction.
    assert!(
        sql.contains(r#"SUM("weight")"#),
        "missing SUM(weight): {sql}"
    );
    assert!(
        sql.contains(
            r#"FROM "rmg_tag" WHERE "rmg_tag"."id" IN (SELECT "tag_id" FROM "rmg_post_tags" WHERE "rmg_post_tags"."post_id" = "rmg_post"."id")"#
        ),
        "M2M sum should reach the target via a junction membership: {sql}"
    );
    assert!(
        sql.contains(r#"AS "tags_sum_weight""#),
        "auto-named alias: {sql}"
    );
}

#[test]
fn m2m_annotate_exists_emits_case_over_junction() {
    let q = Post::objects()
        .annotate_exists("tags")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit").sql;
    assert!(
        sql.contains(r#"CASE WHEN EXISTS (SELECT 1 FROM "rmg_post_tags" WHERE "rmg_post_tags"."post_id" = "rmg_post"."id")"#),
        "M2M annotate_exists CASE/EXISTS: {sql}"
    );
    assert!(sql.contains(r#"AS "tags_exists""#), "alias: {sql}");
}

#[test]
fn m2m_uses_backtick_quoting_on_mysql() {
    let q = Post::objects()
        .where_has("tags")
        .compile()
        .expect("compile");
    let sql = MySql.compile_select(&q).expect("emit").sql;
    assert!(
        sql.contains(
            "EXISTS (SELECT 1 FROM `rmg_post_tags` WHERE `rmg_post_tags`.`post_id` = `rmg_post`.`id`)"
        ),
        "MySQL backtick quoting: {sql}"
    );
}

// ----------------------------------------------------------------- GFK

#[test]
fn gfk_where_has_emits_exists_with_content_type_filter() {
    let q = Article::objects()
        .where_has("comments")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_select(&q).expect("emit").sql;
    assert!(
        sql.contains(
            r#"EXISTS (SELECT 1 FROM "rmg_gcomment" WHERE "rmg_gcomment"."object_pk" = "rmg_article"."id" AND "rmg_gcomment"."content_type_id" = (SELECT "id" FROM "rustango_content_types" WHERE "table" = $1)"#
        ),
        "GFK where_has should EXISTS over the child with a content-type discriminator: {sql}"
    );
}

#[test]
fn gfk_annotate_count_projects_count_with_ct_filter() {
    let q = Article::objects()
        .annotate_count("comments")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit").sql;
    assert!(
        sql.contains(r#"(SELECT COUNT(*) FROM "rmg_gcomment" WHERE "rmg_gcomment"."object_pk" = "rmg_article"."id" AND "rmg_gcomment"."content_type_id" = (SELECT "id" FROM "rustango_content_types" WHERE "table" = $1)) AS "comments_count""#),
        "GFK annotate_count column: {sql}"
    );
}

#[test]
fn gfk_annotate_sum_aggregates_child_column() {
    let q = Article::objects()
        .annotate_sum("comments", "score")
        .compile()
        .expect("compile");
    let sql = Postgres.compile_aggregate(&q).expect("emit").sql;
    assert!(sql.contains(r#"SUM("score")"#), "missing SUM(score): {sql}");
    assert!(
        sql.contains(r#"FROM "rmg_gcomment" WHERE "rmg_gcomment"."object_pk" = "rmg_article"."id" AND "rmg_gcomment"."content_type_id" ="#),
        "GFK sum over child + ct filter: {sql}"
    );
    assert!(
        sql.contains(r#"AS "comments_sum_score""#),
        "auto-named alias: {sql}"
    );
}

#[test]
fn gfk_uses_backtick_quoting_on_mysql() {
    let q = Article::objects()
        .where_has("comments")
        .compile()
        .expect("compile");
    let sql = MySql.compile_select(&q).expect("emit").sql;
    assert!(
        sql.contains("FROM `rmg_gcomment` WHERE `rmg_gcomment`.`object_pk` = `rmg_article`.`id`")
            && sql.contains("(SELECT `id` FROM `rustango_content_types` WHERE `table` = ?)"),
        "MySQL GFK shape: {sql}"
    );
}

// ----------------------------------------------------------- resolution

#[test]
fn unknown_relation_errors_at_compile_time() {
    let err = Post::objects().where_has("nope").compile().unwrap_err();
    assert!(format!("{err}").contains("nope"));
    let err = Article::objects()
        .annotate_count("nope")
        .compile()
        .unwrap_err();
    assert!(format!("{err}").contains("nope"));
}

#[test]
fn metadata_accessors_expose_m2m_and_generic_relations() {
    assert_eq!(Post::SCHEMA.m2m.len(), 1);
    assert_eq!(Post::SCHEMA.m2m[0].name, "tags");
    assert_eq!(Post::SCHEMA.m2m[0].through, "rmg_post_tags");
    let g = Article::generic_reverse_relations();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].name, "comments");
    assert_eq!(g[0].child_schema.table, "rmg_gcomment");
    assert_eq!(g[0].ct_column, "content_type_id");
    assert_eq!(g[0].pk_column, "object_pk");
}
