//! The media tag surface on MySQL 8+ — the first MySQL coverage this
//! module has had.
//!
//! `media_live.rs` and `media_collections_tags_live.rs` are
//! PostgreSQL-typed and `media_sqlite_live.rs` is SQLite, so every
//! dialect branch in `media` had run on two backends out of three. The
//! branches are real: `tag` emits `INSERT IGNORE` on MySQL against
//! `ON CONFLICT DO NOTHING` elsewhere, and `tags_for_many` builds an
//! `IN (…)` whose binds are positional `?` here and `$n` on PostgreSQL.
//!
//! Activated by `MYSQL_TEST_URL` (e.g.
//! `mysql://rustango:rustango@127.0.0.1:3406/rustango_test`). Unset is
//! a skip; set-but-unreachable panics (#1440).
//!
//!   docker compose up -d mysql
//!   export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test
//!   cargo test -p rustango --features mysql,media,testkit --test media_tags_mysql_live

#![cfg(all(feature = "mysql", feature = "media", feature = "testkit"))]

use std::sync::Arc;

use rustango::media::{MediaManager, SaveOpts};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};
use tokio::sync::Mutex;

/// Suite-wide lock. Every test here drops and rebuilds the shared
/// `rustango_media*` tables, so two running in parallel race on the
/// DDL and the loser fails with a MySQL 1050 that has nothing to do
/// with what it was testing.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn manager_or_skip() -> Option<(MediaManager, Pool)> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let p = sqlx::MySqlPool::connect(&url)
        .await
        .expect("connect to MYSQL_TEST_URL");

    // Clean slate. FK checks off around the drops: `rustango_media_tag_links`
    // has an FK into `rustango_media_tags`, and an earlier CI step may
    // have left other framework tables pointing in here too — a drop
    // that fails silently resurfaces later as a 42S01 on the CREATE.
    sqlx::query("SET FOREIGN_KEY_CHECKS = 0")
        .execute(&p)
        .await
        .expect("disable FK checks");
    for tbl in [
        "rustango_media_tag_links",
        "rustango_media_tags",
        "rustango_media",
        "rustango_media_collections",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{tbl}`"))
            .execute(&p)
            .await;
    }
    sqlx::query("SET FOREIGN_KEY_CHECKS = 1")
        .execute(&p)
        .await
        .expect("re-enable FK checks");

    let pool = Pool::Mysql(p);
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework media tables");
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default");
    Some((MediaManager::new_pool(pool.clone(), registry), pool))
}

async fn seed(mgr: &MediaManager) -> i64 {
    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "t/".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: Some(1),
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("seed media");
    match m.id {
        Auto::Set(v) => v,
        _ => panic!("no id"),
    }
}

async fn slugs_of(mgr: &MediaManager, id: i64) -> Vec<String> {
    let mut v: Vec<String> = mgr
        .tags_for(id)
        .await
        .expect("tags_for")
        .into_iter()
        .map(|t| t.slug)
        .collect();
    v.sort();
    v
}

/// `tag` takes the `INSERT IGNORE` branch here, and it has never run
/// against a real MySQL. Tagging twice must not be a duplicate-key
/// error.
#[tokio::test]
async fn tagging_is_idempotent_on_mysql() {
    let _g = live_lock().lock().await;
    let Some((mgr, _pool)) = manager_or_skip().await else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let id = seed(&mgr).await;

    mgr.tag(id, &["alpha", "beta"]).await.expect("tag once");
    mgr.tag(id, &["alpha", "beta"]).await.expect("tag twice");

    assert_eq!(
        slugs_of(&mgr, id).await,
        vec!["alpha".to_owned(), "beta".to_owned()],
        "re-tagging changed the set — `INSERT IGNORE` is the MySQL branch of what \
         `ON CONFLICT DO NOTHING` does elsewhere, and it had never executed"
    );
}

/// `set_tags` is one transaction. On MySQL that means InnoDB, and the
/// delete must roll back with the insert.
#[tokio::test]
async fn a_failed_set_tags_rolls_back_on_mysql() {
    let _g = live_lock().lock().await;
    let Some((mgr, pool)) = manager_or_skip().await else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let id = seed(&mgr).await;
    mgr.tag(id, &["alpha", "beta"]).await.expect("seed tags");

    let before = slugs_of(&mgr, id).await;
    assert_eq!(
        before,
        vec!["alpha".to_owned(), "beta".to_owned()],
        "control: both tags must be set before the failure is simulated"
    );

    // Fail the insert half only. `ensure_tag` writes to a different
    // table, so it still succeeds — the realistic shape is "the new tag
    // exists, the link does not".
    let Pool::Mysql(ref my) = pool else {
        unreachable!("mysql-only suite")
    };
    // Rename the column the INSERT names but the DELETE does not. The
    // delete (`WHERE media_id = ?`) still works; the insert fails with
    // 1054, unknown column. The rename is reversible and keeps the
    // existing rows' values, so `before` survives it.
    //
    // Getting here took three attempts, and the two that failed are
    // worth writing down:
    //
    //  - A `BEFORE INSERT … SIGNAL` trigger — the obvious approach — is
    //    rejected by the prepared-statement protocol (1295), and with
    //    binary logging on it needs `SUPER` (1419), which the test user
    //    does not have here or in CI.
    //  - Any **row-level** failure is swallowed. `tag` emits `INSERT
    //    IGNORE` on MySQL, which downgrades a missing NOT NULL default
    //    (1364) and even a `CHECK` violation (3819) to a warning —
    //    measured, not assumed. So the insert returned `Ok`, the control
    //    assertion below fired, and the test would otherwise have
    //    proven nothing. 1054 is resolved before any row is considered,
    //    which is why it survives `IGNORE`.
    sqlx::query("ALTER TABLE rustango_media_tag_links RENAME COLUMN tag_id TO tag_id_hidden")
        .execute(my)
        .await
        .expect("hide the column the insert names");

    let err = mgr.set_tags(id, &["gamma"]).await;

    // Undo it before asserting, so a failed assertion cannot leave the
    // shared table unusable for the next test.
    sqlx::query("ALTER TABLE rustango_media_tag_links RENAME COLUMN tag_id_hidden TO tag_id")
        .execute(my)
        .await
        .expect("restore the column");

    assert!(
        err.is_err(),
        "control: the insert must fail, or this test proves nothing"
    );
    let after = slugs_of(&mgr, id).await;
    assert_eq!(
        after, before,
        "a failed `set_tags` left the row with {after:?} — neither the old set nor \
         the new one. The delete committed on its own, which on MySQL means the \
         transaction was not doing what it is there to do"
    );
}

/// And the happy path still replaces the set.
#[tokio::test]
async fn set_tags_replaces_the_whole_set_on_mysql() {
    let _g = live_lock().lock().await;
    let Some((mgr, _pool)) = manager_or_skip().await else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };
    let id = seed(&mgr).await;
    mgr.tag(id, &["old-one", "old-two"]).await.expect("seed");

    mgr.set_tags(id, &["new-one"]).await.expect("set_tags");

    assert_eq!(
        slugs_of(&mgr, id).await,
        vec!["new-one".to_owned()],
        "`set_tags` is a wall rather than a replace on MySQL"
    );
}

/// `tags_for_many` builds an `IN (…)` list. MySQL's placeholders are
/// positional `?`, so a bind order that does not follow the SQL's text
/// order is correct on PostgreSQL and wrong here — the trap that
/// produced the named-collection bug earlier in this release.
#[tokio::test]
async fn tags_for_many_batches_correctly_on_mysql() {
    let _g = live_lock().lock().await;
    let Some((mgr, _pool)) = manager_or_skip().await else {
        eprintln!("skipping — set MYSQL_TEST_URL");
        return;
    };

    let mut ids = Vec::new();
    for i in 0..4 {
        let id = seed(&mgr).await;
        mgr.tag(id, &[format!("t{i}").as_str()]).await.expect("tag");
        ids.push(id);
    }

    let batched = mgr.tags_for_many(&ids).await.expect("tags_for_many");
    assert_eq!(batched.len(), 4, "a media id with tags went missing");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            batched.get(id).map(Vec::as_slice),
            Some([format!("t{i}")].as_slice()),
            "batched tags disagree with what was written for {id} — an `IN (…)` \
             whose binds are not in text order is correct on PostgreSQL and wrong \
             on MySQL, and nothing else exercises this path here"
        );
    }

    // Agrees with the per-row call it replaces.
    for id in &ids {
        let one: Vec<String> = mgr
            .tags_for(*id)
            .await
            .expect("per-row")
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert_eq!(batched.get(id), Some(&one), "batched != per-row for {id}");
    }
}
