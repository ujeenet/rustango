//! Django-parity #432 — factory_boy-shape `Factory` + `Sequence`.
//!
//! Verifies the `test_factory::Sequence` counter is deterministic
//! across `build()` calls and that `Factory::build_batch` can drive
//! a real SQLite insert loop end-to-end.

#![cfg(feature = "sqlite")]

use rustango::core::SqlValue;
use rustango::sql::Pool;
use rustango::test_factory::{Factory, Sequence};

#[derive(Debug, Clone, Default, PartialEq)]
struct User {
    username: String,
    email: String,
}

struct UserFactory {
    seq: Sequence<u64>,
}

impl Default for UserFactory {
    fn default() -> Self {
        Self {
            seq: Sequence::new(|n| n),
        }
    }
}

impl Factory for UserFactory {
    type Item = User;
    fn build(&self) -> User {
        let n = self.seq.next();
        User {
            username: format!("user-{n}"),
            email: format!("user-{n}@example.com"),
        }
    }
}

#[test]
fn build_batch_produces_unique_usernames() {
    let f = UserFactory::default();
    let batch = f.build_batch(4);
    assert_eq!(batch.len(), 4);
    assert_eq!(batch[0].username, "user-0");
    assert_eq!(batch[3].username, "user-3");
    let mut names: Vec<_> = batch.iter().map(|u| u.username.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 4, "every username should be unique");
}

#[test]
fn sequence_threads_share_counter() {
    use std::sync::Arc;
    let seq: Arc<Sequence<u64>> = Arc::new(Sequence::new(|n| n));
    let handles: Vec<_> = (0..32)
        .map(|_| {
            let seq = Arc::clone(&seq);
            std::thread::spawn(move || seq.next())
        })
        .collect();
    let mut values: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    values.sort_unstable();
    assert_eq!(values, (0..32).collect::<Vec<_>>());
}

#[tokio::test]
async fn factory_drives_sqlite_inserts() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "fb_users" (
            "id"       INTEGER PRIMARY KEY AUTOINCREMENT,
            "username" TEXT NOT NULL UNIQUE,
            "email"    TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");

    let f = UserFactory::default();
    for u in f.build_batch(10) {
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "fb_users" ("username", "email") VALUES (?, ?)"#,
            vec![
                SqlValue::String(u.username.clone()),
                SqlValue::String(u.email.clone()),
            ],
        )
        .await
        .expect("insert");
    }

    // Re-fetch and confirm every row landed with a unique username.
    use sqlx::Row;
    let rows = match &pool {
        Pool::Sqlite(sq) => sqlx::query(r#"SELECT "username" FROM "fb_users" ORDER BY "id""#)
            .fetch_all(sq)
            .await
            .expect("fetch_all"),
        #[allow(unreachable_patterns)]
        _ => unreachable!("test is sqlite-only"),
    };
    let names: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<String, _>("username").unwrap())
        .collect();
    assert_eq!(names.len(), 10);
    assert_eq!(names[0], "user-0");
    assert_eq!(names[9], "user-9");
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        10,
        "every inserted row should have a unique username"
    );
}
