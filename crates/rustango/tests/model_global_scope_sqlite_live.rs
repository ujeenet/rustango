#![cfg(feature = "sqlite")]
//! Live SQLite tests for Eloquent-shape **global scopes** —
//! auto-applied query filters declared via
//! `#[rustango(global_scope(name = "...", apply = path::to::fn))]`
//! on the model struct.
//!
//! Closes issue [#820](https://github.com/ujeenet/rustango/issues/820).
//!
//! The substrate covers:
//!
//! 1. **Auto-application** — every QuerySet built via `Post::objects()`
//!    / `Post::all(&pool)` carries the scope's WHERE without the
//!    caller chaining `.filter(...)` explicitly.
//! 2. **Per-name opt-out** — `qs.without_global_scope("active")`
//!    suppresses one scope; other scopes (if any) keep applying.
//! 3. **Wholesale opt-out** — `qs.without_global_scopes()` returns the
//!    same WHERE you'd get on a model with no scopes declared.
//! 4. **Apply across query verbs** — SELECT (`fetch` /
//!    `Model::all`) and aggregate (`count` / `Model::count`) both
//!    fold the scope in. DELETE is exercised via the free
//!    `rustango::sql::delete_pool` to prove `compile_delete()` honors
//!    the scope too.
//! 5. **Composition with user filters** — `qs.filter(...)` on top of a
//!    scoped queryset AND-composes; no scope is silently dropped.

use rustango::core::{Filter, Op, SqlValue, WhereExpr};
use rustango::sql::{delete_pool, sqlx, Auto, CounterPool as _, FetcherPool as _, Pool};
use rustango::Model;

/// The scope's filter constructor — referenced from the model's
/// `#[rustango(global_scope(... apply = active_only))]` attribute.
/// Returns `is_active = true` so every queryset for `Post` is
/// implicitly `Post::objects().filter(is_active = true)`.
fn active_only() -> WhereExpr {
    WhereExpr::Predicate(Filter {
        column: "is_active",
        op: Op::Eq,
        value: SqlValue::Bool(true),
    })
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gs_post", global_scope(name = "active", apply = active_only))]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub is_active: bool,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE gs_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            is_active INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (t, active) in [
        ("alpha", true),
        ("beta", false),
        ("gamma", true),
        ("delta", false),
        ("epsilon", true),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            is_active: active,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[test]
fn schema_carries_one_global_scope() {
    use rustango::core::Model as _;
    let scopes = Post::SCHEMA.global_scopes;
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].name, "active");
    // The fn pointer round-trips through the schema — invoking it
    // returns the same WHERE we'd build by hand.
    let expr = (scopes[0].apply)();
    match expr {
        WhereExpr::Predicate(f) => {
            assert_eq!(f.column, "is_active");
            assert_eq!(f.op, Op::Eq);
            assert_eq!(f.value, SqlValue::Bool(true));
        }
        _ => panic!("expected Predicate, got {expr:?}"),
    }
}

#[tokio::test]
async fn default_queryset_hides_inactive_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    // `Post::all(&pool)` is the macro-emitted shortcut over
    // `Post::objects().fetch(&pool)` — the scope should fold in
    // here just as in the explicit chain.
    let visible = Post::all(&pool).await.unwrap();
    assert_eq!(visible.len(), 3, "scope must filter to active rows only");
    for row in &visible {
        assert!(row.is_active);
    }
}

#[tokio::test]
async fn without_global_scope_by_name_sees_every_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let all = Post::objects()
        .without_global_scope("active")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn without_global_scopes_sees_every_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let all = Post::objects()
        .without_global_scopes()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn unknown_scope_name_is_silently_ignored() {
    let pool = make_pool().await;
    seed(&pool).await;
    // `nope` doesn't exist on the model — Eloquent silently ignores
    // unknown scope names; rustango matches that to keep call sites
    // robust against scope renames.
    let visible = Post::objects()
        .without_global_scope("nope")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(visible.len(), 3);
}

#[tokio::test]
async fn count_honors_global_scope() {
    let pool = make_pool().await;
    seed(&pool).await;
    let active_count = Post::count(&pool).await.unwrap();
    assert_eq!(active_count, 3);

    let total = Post::objects()
        .without_global_scopes()
        .count(&pool)
        .await
        .unwrap();
    assert_eq!(total, 5);
}

#[tokio::test]
async fn user_filters_compose_with_global_scope() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Scoped + user filter — only matches `alpha` since it's the only
    // active row whose title equals "alpha".
    let rows = Post::objects()
        .filter("title", "alpha")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "alpha");

    // Filter targeting an inactive row — scoped queryset MUST return
    // empty (scope hides it); unscoped MUST find it.
    let beta_scoped = Post::objects()
        .filter("title", "beta")
        .fetch(&pool)
        .await
        .unwrap();
    assert!(beta_scoped.is_empty(), "scope must hide inactive `beta`");
    let beta_unscoped = Post::objects()
        .without_global_scopes()
        .filter("title", "beta")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(beta_unscoped.len(), 1);
}

#[tokio::test]
async fn compile_delete_honors_global_scope() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Wholesale delete via the scoped queryset must only remove the
    // active rows. The two inactive rows survive — proves the scope
    // folds into the WHERE on `compile_delete()` not just SELECT.
    let query = Post::objects().compile_delete().unwrap();
    let removed = delete_pool(&pool, &query).await.unwrap();
    assert_eq!(removed, 3, "delete must respect the global scope");

    let survivors = Post::objects()
        .without_global_scopes()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(survivors.len(), 2);
    for s in &survivors {
        assert!(!s.is_active);
    }
}
