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

/// A second database whose `content_type` ids are deliberately offset
/// from the first's, which is what a real second tenant looks like:
/// its sequence is its own, and it was seeded at its own time against
/// its own model set.
///
/// The filler rows go in **before** `ensure_seeded`, so the real rows
/// land higher. Inserting them afterwards leaves the seeded ids equal
/// across pools and makes any cross-tenant assertion vacuous.
async fn sqlite_pool_with_offset(offset: usize) -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool"),
    );
    contenttypes::ensure_table(&pool)
        .await
        .expect("content type table");
    for i in 0..offset {
        sqlx::query(
            "INSERT INTO rustango_content_types (app_label, model_name, \"table\") \
             VALUES (?, ?, ?)",
        )
        .bind(format!("ct_bc_filler{i}"))
        .bind(format!("filler{i}"))
        .bind(format!("ct_bc_filler{i}"))
        .execute(pool.as_sqlite().expect("sqlite"))
        .await
        .expect("shift the sequence");
    }
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
    let sq = pool.as_sqlite().expect("sqlite pool");
    sqlx::query("DROP TABLE rustango_content_types")
        .execute(sq)
        .await
        .expect("drop");

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
    let sq = pool.as_sqlite().expect("sqlite pool");
    sqlx::query("DROP TABLE rustango_content_types")
        .execute(sq)
        .await
        .expect("drop");
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
    let sq = pool.as_sqlite().expect("sqlite pool");
    sqlx::query(
        "INSERT INTO rustango_content_types (app_label, model_name, \"table\") \
         VALUES ('ghost_app', 'ghost_model', 'ghost_table')",
    )
    .execute(sq)
    .await
    .expect("insert");

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

/// Two databases must not share a cached `ContentType` id (#1533).
///
/// The id comes from each database's own sequence, so the same natural
/// key is a different number in each. Keyed on the natural key alone —
/// as it was — the first pool to warm the entry decided what the second
/// was handed, and three public generic-FK entry points reach this, so
/// the wrong id lands in real junction rows.
///
/// The assertion is not "the two ids differ". That would pass on an
/// over-partitioned key that is still wrong, and it would pass on a key
/// that changes every call. It is **`b` gets what an uncached read of
/// `b` returns** — the cache must be transparent, which is the only
/// property that actually matters here.
#[tokio::test]
async fn a_second_database_does_not_inherit_the_first_ones_id() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();

    let a = sqlite_pool().await;
    // `b`'s sequence is shifted **before** it is seeded, so its `post`
    // lands on a different id from `a`'s. Doing this after seeding is
    // useless — the row already has its id, both pools agree, and a
    // shared cache entry is then indistinguishable from a correct one.
    // The first draft did exactly that and passed with the fix
    // reverted.
    let b = sqlite_pool_with_offset(3).await;
    contenttypes::clear_cache();

    // Warm from `a` first — the tenant that "gets there first".
    let from_a = ContentType::get_by_natural_key(&a, "ct_bc_blog", "post")
        .await
        .expect("a lookup")
        .expect("seeded in a");

    let from_b = ContentType::get_by_natural_key(&b, "ct_bc_blog", "post")
        .await
        .expect("b lookup")
        .expect("seeded in b");

    let b_truth = ContentType::by_natural_key(&b.clone().into(), "ct_bc_blog", "post")
        .await
        .expect("b uncached")
        .expect("seeded in b");

    assert_eq!(
        from_b.id.get(),
        b_truth.id.get(),
        "the cached read for pool b must equal an uncached read of b. \
         Getting a's id ({:?}) here is the cross-tenant defect: the cache \
         was keyed on the natural key alone, so whoever warmed it first \
         decided every other database's answer (#1533).",
        from_a.id.get()
    );
}

/// `clear_cache_for` drops one database's entries and leaves the rest.
#[tokio::test]
async fn clearing_one_scope_leaves_the_other() {
    let _g = cache_lock().lock().await;
    contenttypes::clear_cache();

    let a = sqlite_pool().await;
    let b = sqlite_pool().await;

    let _ = ContentType::get_by_natural_key(&a, "ct_bc_blog", "post").await;
    let _ = ContentType::get_by_natural_key(&b, "ct_bc_blog", "post").await;

    // Drop `a`'s table so a cache miss on `a` would error, then clear
    // only `b`. `a` must still answer from cache; `b` must re-read and
    // succeed. Asserting on a *dropped table* is what makes "served
    // from cache" observable at all.
    sqlx::query("DROP TABLE rustango_content_types")
        .execute(a.as_sqlite().expect("sqlite"))
        .await
        .expect("drop a's table");

    contenttypes::clear_cache_for(&b);

    assert!(
        ContentType::get_by_natural_key(&a, "ct_bc_blog", "post")
            .await
            .expect("a still cached")
            .is_some(),
        "clearing b's scope must not evict a — a's table is gone, so a \
         miss here would surface as an error rather than a wrong id"
    );
    assert!(
        ContentType::get_by_natural_key(&b, "ct_bc_blog", "post")
            .await
            .expect("b re-reads")
            .is_some(),
        "b must re-read from its own database after its scope was cleared"
    );
}
