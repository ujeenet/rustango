//! Framework writes that open their own transaction, inside a caller's
//! `atomic` block on the same pool: fixtures join it and roll back with
//! it; the registry and the cache stay independent and survive it.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "tenancy",
    feature = "cache",
    feature = "testkit"
))]

use rustango::cache::{Cache as _, DatabaseCache};
use rustango::core::Column as _;
use rustango::fixtures::{load_all_pool, Fixture};
use rustango::sql::{Auto, ExecError, FetcherPool as _, Pool, SqlError};
use rustango::tenancy::Operator;
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "asite_item")]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 32)]
    pub label: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn bail() -> ExecError {
    ExecError::Sql(SqlError::EmptyInList)
}

async fn items(pool: &Pool) -> Vec<i64> {
    let rows: Vec<Item> = Item::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

/// SQLite takes the write lock for the whole block, so a write of its own
/// on the same database waits for the block: there is nothing to survive.
fn one_writer(pool: &Pool) -> bool {
    pool.dialect().name() == "sqlite"
}

/// `load_all_pool` joins the caller's block and rolls back with it.
async fn fixtures_roll_back_with_the_callers_block(pool: &Pool) {
    let row = |id: i64| {
        serde_json::json!({ "id": id, "label": format!("f{id}") })
            .as_object()
            .unwrap()
            .clone()
    };
    let fixture = Fixture::new("items").with_row(row(1)).with_row(row(2));
    let q = pool.clone();
    let res: Result<(), ExecError> = rustango::atomic!(pool, |_tx| {
        let n = load_all_pool(&[("asite_item", &fixture)], &q)
            .await
            .expect("fixture");
        assert_eq!(n, 2);
        Err(bail())
    })
    .await;
    assert!(res.is_err());
    assert_eq!(
        items(pool).await,
        Vec::<i64>::new(),
        "the fixture committed"
    );
}

async fn seed_operator(pool: &Pool, name: &str) -> Operator {
    let mut op = Operator {
        id: Auto::default(),
        username: name.to_owned(),
        password_hash: "x".into(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert(pool).await.expect("seed operator");
    op
}

/// A registry change commits on its own: the caller's rollback keeps it.
async fn registry_survives_the_callers_rollback(pool: &Pool) {
    if one_writer(pool) {
        return;
    }
    let tag = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    seed_operator(pool, &format!("keep-{tag}")).await;
    let mut gone = seed_operator(pool, &format!("gone-{tag}")).await;
    let id = *gone.id.get().unwrap();
    let q = pool.clone();
    let res: Result<(), ExecError> = rustango::atomic!(pool, |_tx| {
        Item {
            id: 1,
            label: "mine".into(),
        }
        .insert(&q)
        .await?;
        rustango::tenancy::operators::set_active(&q, &mut gone, false, None)
            .await
            .expect("deactivate");
        Err(bail())
    })
    .await;
    assert!(res.is_err());
    assert_eq!(items(pool).await, Vec::<i64>::new(), "the block's own row");
    let after = Operator::objects()
        .where_(Operator::id.eq(id))
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    assert!(
        !after.active,
        "the registry change rolled back with the caller"
    );
}

/// A cache write commits on its own: the caller's rollback keeps it.
async fn cache_survives_the_callers_rollback(pool: &Pool) {
    if one_writer(pool) {
        return;
    }
    let cache = DatabaseCache::new(pool.clone(), "asite_cache");
    cache.ensure_table().await.expect("cache table");
    cache.clear().await.expect("clear");
    let (c, q) = (cache.clone(), pool.clone());
    let res: Result<(), ExecError> = rustango::atomic!(pool, |_tx| {
        Item {
            id: 1,
            label: "mine".into(),
        }
        .insert(&q)
        .await?;
        c.set("k", "v", None).await.expect("set");
        Err(bail())
    })
    .await;
    assert!(res.is_err());
    assert_eq!(items(pool).await, Vec::<i64>::new(), "the block's own row");
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        fixtures_roll_back_with_the_callers_block,
        registry_survives_the_callers_rollback,
        cache_survives_the_callers_rollback,
    ],
}
