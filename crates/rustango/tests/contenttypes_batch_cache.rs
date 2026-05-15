//! Tests for `ContentType::for_models_pool` batch lookup and the
//! `for_model_cached_pool` / `by_natural_key_cached_pool` cache layer
//! (issue #35). Runs against in-memory SQLite — no infra needed.

#![cfg(feature = "sqlite")]

use rustango::contenttypes::{self, ContentType};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

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
    contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("ensure_seeded_pool");
    pool
}

/// Batch lookup returns one entry per requested pair that exists.
/// Both `&str` literals and `String` values are accepted.
#[tokio::test]
async fn for_models_pool_returns_matching_rows() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;

    // &str literals — the primary ergonomic form.
    let cts =
        ContentType::for_models_pool(&pool, [("ct_bc_blog", "post"), ("ct_bc_blog", "author")])
            .await
            .expect("for_models_pool with &str pairs");
    assert_eq!(cts.len(), 2, "both &str pairs should resolve");
    assert!(cts.contains_key(&("ct_bc_blog".into(), "post".into())));
    assert!(cts.contains_key(&("ct_bc_blog".into(), "author".into())));

    // String values also accepted.
    let cts2 = ContentType::for_models_pool(
        &pool,
        [
            ("ct_bc_blog".to_string(), "post".to_string()),
            ("ct_bc_blog".to_string(), "author".to_string()),
        ],
    )
    .await
    .expect("for_models_pool with String pairs");
    assert_eq!(cts2.len(), 2, "both String pairs should resolve");
}

/// Unknown pairs are silently omitted from the result map — same
/// shape Django's `get_for_models` returns when a model isn't
/// migrated yet.
#[tokio::test]
async fn for_models_pool_omits_unknown_pairs() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let cts =
        ContentType::for_models_pool(&pool, [("ct_bc_blog", "post"), ("nonexistent", "nope")])
            .await
            .expect("for_models_pool");
    assert_eq!(cts.len(), 1, "only the registered pair should appear");
    assert!(cts.contains_key(&("ct_bc_blog".into(), "post".into())));
    assert!(!cts.contains_key(&("nonexistent".into(), "nope".into())));
}

/// Empty input → empty output, no DB round trip (caller can skip
/// the lookup entirely when they have nothing to ask about).
#[tokio::test]
async fn for_models_pool_empty_input_is_empty_output() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let cts = ContentType::for_models_pool(&pool, std::iter::empty::<(String, String)>())
        .await
        .expect("empty");
    assert!(cts.is_empty());
}

/// Cached lookup returns the same ContentType row as the uncached
/// path — the cache doesn't change semantics, only speed.
#[tokio::test]
async fn by_natural_key_cached_matches_uncached() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let uncached = ContentType::by_natural_key_pool(&pool, "ct_bc_blog", "post")
        .await
        .expect("uncached lookup")
        .expect("Post is seeded");
    let cached = ContentType::by_natural_key_cached_pool(&pool, "ct_bc_blog", "post")
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
async fn cached_lookup_serves_from_cache_on_repeat() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let _ = ContentType::by_natural_key_cached_pool(&pool, "ct_bc_blog", "post")
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

    let second = ContentType::by_natural_key_cached_pool(&pool, "ct_bc_blog", "post")
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
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let _ = ContentType::by_natural_key_cached_pool(&pool, "ct_bc_blog", "post")
        .await
        .expect("populate cache");

    // Drop + recreate the table empty (no seed rows).
    if let Pool::Sqlite(sq) = &pool {
        sqlx::query("DROP TABLE rustango_content_types")
            .execute(sq)
            .await
            .expect("drop");
    }
    contenttypes::ensure_table_pool(&pool)
        .await
        .expect("recreate empty");

    // clear_cache → next call hits the empty table → None.
    contenttypes::clear_cache();
    let after = ContentType::by_natural_key_cached_pool(&pool, "ct_bc_blog", "post")
        .await
        .expect("lookup ok");
    assert!(after.is_none(), "cache cleared + table empty → None");
}

/// Negative result (`None`) is NOT cached — so a re-seed isn't
/// blocked by a stale negative entry.
#[tokio::test]
async fn negative_results_are_not_cached() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    // First lookup against an unknown pair → None.
    let r1 = ContentType::by_natural_key_cached_pool(&pool, "ghost_app", "ghost_model")
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
    let r2 = ContentType::by_natural_key_cached_pool(&pool, "ghost_app", "ghost_model")
        .await
        .expect("ok")
        .expect("Some now");
    assert_eq!(r2.app_label, "ghost_app");
    assert_eq!(r2.model_name, "ghost_model");
}

/// `for_model_cached_pool::<T>` returns the same row as the uncached
/// `for_model_pool::<T>` and uses the natural-key cache.
#[tokio::test]
async fn for_model_cached_pool_resolves_type() {
    contenttypes::clear_cache();
    let pool = sqlite_pool().await;
    let cached = ContentType::for_model_cached_pool::<Post>(&pool)
        .await
        .expect("cached for_model")
        .expect("Post is seeded");
    let uncached = ContentType::for_model_pool::<Post>(&pool)
        .await
        .expect("uncached for_model")
        .expect("Post is seeded");
    assert_eq!(cached.id.get(), uncached.id.get());
    assert_eq!(cached.app_label, "ct_bc_blog");
    assert_eq!(cached.model_name, "post");
}
