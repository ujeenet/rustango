//! Tests for `ContentType::get_for_models` batch lookup and the
//! `get_for_model` / `get_by_natural_key` cache layer
//! (issue #35). Runs against in-memory SQLite — no infra needed.
//!
//! ## Why the suite-wide serializing mutex
//!
//! The ContentType cache (`contenttypes::clear_cache` / the static
//! HashMap behind `get_*` methods) is **process-global**. Under
//! cargo's default parallel test harness, two tests racing on
//! `clear_cache()` between another test's "populate" and "assert HIT"
//! calls would evict the entry and force a DB hit — which then fails
//! on a dropped table in the table-drop tests below. The lock makes
//! every test in this file run sequentially against the shared cache.

#![cfg(feature = "sqlite")]

use std::sync::OnceLock;

use rustango::contenttypes::{self, ContentType};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

/// Suite-wide lock — gates every test against the process-global
/// ContentType cache so the cache state stays coherent across the
/// "clear → populate → assert" pattern each test follows.
fn cache_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_bc_post")]
#[rustango(app = "ct_bc_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_bc_author")]
#[rustango(app = "ct_bc_blog")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
}

async fn sqlite_pool() -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool"),
    );
    contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded_pool");
    pool
}

/// Batch lookup returns one entry per requested pair that exists.
/// Both `&str` literals and `String` values are accepted.
#[tokio::test]
async fn get_for_models_returns_matching_rows() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;

    // &str literals — the primary ergonomic form.
    let cts =
        ContentType::get_for_models(&pool, [("ct_bc_blog", "post"), ("ct_bc_blog", "author")])
            .await
            .expect("get_for_models with &str pairs");
    assert_eq!(cts.len(), 2, "both &str pairs should resolve");
    assert!(cts.contains_key(&("ct_bc_blog".into(), "post".into())));
    assert!(cts.contains_key(&("ct_bc_blog".into(), "author".into())));

    // String values also accepted.
    let cts2 = ContentType::get_for_models(
        &pool,
        [
            ("ct_bc_blog".to_string(), "post".to_string()),
            ("ct_bc_blog".to_string(), "author".to_string()),
        ],
    )
    .await
    .expect("get_for_models with String pairs");
    assert_eq!(cts2.len(), 2, "both String pairs should resolve");
}

/// Unknown pairs are silently omitted from the result map — same
/// shape Django's `get_for_models` returns when a model isn't
/// migrated yet.
#[tokio::test]
async fn get_for_models_omits_unknown_pairs() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let cts = ContentType::get_for_models(&pool, [("ct_bc_blog", "post"), ("nonexistent", "nope")])
        .await
        .expect("get_for_models");
    assert_eq!(cts.len(), 1, "only the registered pair should appear");
    assert!(cts.contains_key(&("ct_bc_blog".into(), "post".into())));
    assert!(!cts.contains_key(&("nonexistent".into(), "nope".into())));
}

/// Empty input → empty output, no DB round trip (caller can skip
/// the lookup entirely when they have nothing to ask about).
#[tokio::test]
async fn get_for_models_empty_input_is_empty_output() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let cts = ContentType::get_for_models(&pool, std::iter::empty::<(String, String)>())
        .await
        .expect("empty");
    assert!(cts.is_empty());
}

/// Cached lookup returns the same ContentType row as the uncached
/// path — the cache doesn't change semantics, only speed.
#[tokio::test]
async fn get_by_natural_key_matches_uncached() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let uncached = ContentType::by_natural_key(&pool, "ct_bc_blog", "post")
        .await
        .expect("uncached lookup")
        .expect("Post is seeded");
    let cached = ContentType::get_by_natural_key(&pool, "ct_bc_blog", "post")
        .await
        .expect("cached lookup")
        .expect("Post is seeded");
    assert_eq!(uncached.id.get(), cached.id.get());
    assert_eq!(uncached.app_label, cached.app_label);
    assert_eq!(uncached.model_name, cached.model_name);
    assert_eq!(uncached.table, cached.table);
}

/// Second cached call doesn't re-query the DB — we prove this by
/// dropping the table after the first call and seeing the second
/// still succeed.
#[tokio::test]
async fn get_by_natural_key_serves_from_cache_on_repeat() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let _ = ContentType::get_by_natural_key(&pool, "ct_bc_blog", "post")
        .await
        .expect("first call populates cache")
        .expect("post seeded");

    // Drop the source table — the cache should still serve the row.
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query("DROP TABLE rustango_content_types")
            .execute(sq)
            .await
            .expect("drop");
    }

    let second = ContentType::get_by_natural_key(&pool, "ct_bc_blog", "post")
        .await
        .expect("second call hits cache, no DB")
        .expect("cache still has it");
    assert_eq!(second.app_label, "ct_bc_blog");
    assert_eq!(second.model_name, "post");
}

/// `clear_cache()` evicts entries — the next call goes back to the DB
/// (which after the table drop above means a fresh seed must re-occur).
#[tokio::test]
async fn clear_cache_forces_db_round_trip_again() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let _ = ContentType::get_by_natural_key(&pool, "ct_bc_blog", "post")
        .await
        .expect("populate cache");

    // Drop + recreate the table empty (no seed rows).
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query("DROP TABLE rustango_content_types")
            .execute(sq)
            .await
            .expect("drop");
    }
    contenttypes::ensure_table(&pool)
        .await
        .expect("recreate empty");

    // clear_cache → next call hits the empty table → None.
    contenttypes::clear_cache();
    let after = ContentType::get_by_natural_key(&pool, "ct_bc_blog", "post")
        .await
        .expect("lookup ok");
    assert!(after.is_none(), "cache cleared + table empty → None");
}

/// Negative result (`None`) is NOT cached — so a re-seed isn't
/// blocked by a stale negative entry.
#[tokio::test]
async fn negative_results_are_not_cached() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    // First lookup against an unknown pair → None.
    let r1 = ContentType::get_by_natural_key(&pool, "ghost_app", "ghost_model")
        .await
        .expect("ok");
    assert!(r1.is_none());

    // Insert a row for the previously-missing pair manually.
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query(
            "INSERT INTO rustango_content_types (app_label, model_name, \"table\") \
             VALUES ('ghost_app', 'ghost_model', 'ghost_table')",
        )
        .execute(sq)
        .await
        .expect("insert");
    }

    // Second lookup must find it (the None wasn't cached).
    let r2 = ContentType::get_by_natural_key(&pool, "ghost_app", "ghost_model")
        .await
        .expect("ok")
        .expect("Some now");
    assert_eq!(r2.app_label, "ghost_app");
    assert_eq!(r2.model_name, "ghost_model");
}

/// `get_for_model::<T>` returns the same row as the uncached
/// `for_model::<T>` and uses the natural-key cache.
#[tokio::test]
async fn get_for_model_resolves_type() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let cached = ContentType::get_for_model::<Post>(&pool)
        .await
        .expect("cached for_model")
        .expect("Post is seeded");
    let uncached = ContentType::for_model::<Post>(&pool)
        .await
        .expect("uncached for_model")
        .expect("Post is seeded");
    assert_eq!(cached.id.get(), uncached.id.get());
    assert_eq!(cached.app_label, "ct_bc_blog");
    assert_eq!(cached.model_name, "post");
}
