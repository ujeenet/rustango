#![cfg(feature = "postgres")]
//! Live PG test for `CASE WHEN … THEN … ELSE … END` conditional
//! expressions (issue #4). Confirms the emitted SQL actually executes
//! end-to-end and produces the right values — the emission tests pin
//! the SQL strings, this pins the runtime semantics.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::case::{case, value};
use rustango::core::funcs::lower;
use rustango::core::{Column as _, F};
use rustango::sql::{sqlx, Auto, Fetcher, Updater};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "case_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 20)]
    pub status: String,
    #[rustango(max_length = 200)]
    pub title: String,
    pub views: i64,
    #[rustango(max_length = 50)]
    pub label: String,
    pub priority: i64,
    // Nullable target so `case_without_else_returns_null_for_unmatched_rows`
    // can write a no-default CASE and observe NULL on unmatched rows.
    #[rustango(max_length = 50)]
    pub maybe_label: Option<String>,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "case_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "case_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "status" VARCHAR(20) NOT NULL,
            "title" VARCHAR(200) NOT NULL,
            "views" BIGINT NOT NULL DEFAULT 0,
            "label" VARCHAR(50) NOT NULL DEFAULT '',
            "priority" BIGINT NOT NULL DEFAULT 99,
            "maybe_label" VARCHAR(50)
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();

    // 4 rows covering every status branch we want to assert on.
    for (status, title, views) in [
        ("draft", "First Draft", 0_i64),
        ("review", "Pending Review", 5),
        ("published", "Live Post", 100),
        ("published", "Hot Post", 10_000),
    ] {
        sqlx::query(&format!(
            r#"INSERT INTO "case_post" ("status", "title", "views") VALUES ('{status}', '{title}', {views})"#
        ))
        .execute(pool)
        .await
        .unwrap();
    }
}

/// Single WHEN + ELSE: the canonical "derive a label from status" recipe.
#[tokio::test]
async fn case_writes_branch_value_per_row() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // SET label = CASE WHEN status='draft' THEN 'Draft' ELSE 'Live' END
    Post::objects()
        .update()
        .set_expr(
            "label",
            case()
                .when(Post::status.eq("draft"), value("Draft"))
                .default(value("Live")),
        )
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    let labels: Vec<&str> = rows.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["Draft", "Live", "Live", "Live"],
        "draft → Draft, everything else → Live: {labels:?}",
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "case_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

/// Custom-ordering target: derive a priority key per status, sort by it.
/// This is one of the issue #4 acceptance criteria ("custom orderings").
#[tokio::test]
async fn case_drives_a_derived_ordering_column() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // Custom rank: published=0, review=1, draft=2.
    Post::objects()
        .update()
        .set_expr(
            "priority",
            case()
                .when(Post::status.eq("published"), 0_i64)
                .when(Post::status.eq("review"), 1_i64)
                .when(Post::status.eq("draft"), 2_i64)
                .default(99_i64),
        )
        .execute(&pool)
        .await
        .unwrap();

    let ordered: Vec<Post> = Post::objects()
        .order_by(&[("priority", false), ("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    let statuses: Vec<&str> = ordered.iter().map(|p| p.status.as_str()).collect();
    // published rows (both) first, then review, then draft.
    assert_eq!(
        statuses,
        vec!["published", "published", "review", "draft"],
        "custom ordering should run published → review → draft: {statuses:?}",
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "case_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

/// Conditional default on update: fall back to lower(title) when label
/// is blank. Tests #4's "computed defaults in update()" acceptance and
/// confirms Case composes with function-call THEN branches (also covers
/// the F() column-ref-as-default fallthrough).
#[tokio::test]
async fn case_with_function_in_then_branch_executes() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // SET label = CASE WHEN status='draft' THEN LOWER(title) ELSE title END
    Post::objects()
        .update()
        .set_expr(
            "label",
            case()
                .when(Post::status.eq("draft"), lower(F("title")))
                .default(F("title")),
        )
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // First row has status='draft', title='First Draft', should land as 'first draft'.
    assert_eq!(rows[0].label, "first draft");
    // Rest are non-draft, label should match the title verbatim.
    assert_eq!(rows[1].label, "Pending Review");
    assert_eq!(rows[2].label, "Live Post");
    assert_eq!(rows[3].label, "Hot Post");

    sqlx::query(r#"DROP TABLE IF EXISTS "case_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

/// AND/OR composition in the WHEN predicate — confirms TypedExpr lowers
/// to WhereExpr correctly through the Case path.
#[tokio::test]
async fn case_with_and_or_predicate_executes() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // SET label = CASE
    //   WHEN status='published' AND views > 1000 THEN 'viral'
    //   WHEN status='published' THEN 'live'
    //   ELSE 'pending' END
    let cond_viral = Post::status.eq("published").and(Post::views.gt(1000_i64));
    Post::objects()
        .update()
        .set_expr(
            "label",
            case()
                .when(cond_viral, value("viral"))
                .when(Post::status.eq("published"), value("live"))
                .default(value("pending")),
        )
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // Row 1 draft → pending, row 2 review → pending,
    // row 3 published+views=100 → live, row 4 published+views=10000 → viral.
    let labels: Vec<&str> = rows.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(labels, vec!["pending", "pending", "live", "viral"]);

    sqlx::query(r#"DROP TABLE IF EXISTS "case_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

/// No-default behaviour: a CASE without `.default(...)` returns NULL
/// for any row that matches none of the `WHEN` branches. Writes the
/// CASE to the nullable `maybe_label` column, then reads back and
/// asserts the unmatched rows came back as `None` (not the empty
/// string, not a fallback).
#[tokio::test]
async fn case_without_else_returns_null_for_unmatched_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // SET maybe_label = CASE WHEN status = 'draft' THEN 'Draft' END
    // — three of the four rows are non-draft and should land as NULL.
    Post::objects()
        .update()
        .set_expr(
            "maybe_label",
            case().when(Post::status.eq("draft"), value("Draft")),
        )
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // Row 1 (draft) hits the WHEN branch; rows 2-4 fall through and
    // get NULL because there is no ELSE.
    assert_eq!(rows[0].maybe_label.as_deref(), Some("Draft"));
    assert_eq!(rows[1].maybe_label, None);
    assert_eq!(rows[2].maybe_label, None);
    assert_eq!(rows[3].maybe_label, None);

    sqlx::query(r#"DROP TABLE IF EXISTS "case_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
