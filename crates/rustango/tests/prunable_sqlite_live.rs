#![cfg(feature = "sqlite")]
//! Live SQLite tests for [`rustango::prunable`] — issue #822.
//!
//! Confirms:
//! * `Prunable::prune_queryset` + `register_prunable!` register the
//!   model with the inventory walker.
//! * `prune_all` deletes the matching rows; non-matching rows
//!   survive.
//! * `prune_pretend` counts without deleting.
//! * `PruneOptions::only` / `except` honored.

use rustango::prunable::{prune_all, prune_pretend, registered_names, Prunable, PruneOptions};
use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "prl_audit")]
#[allow(dead_code)]
pub struct AuditEntry {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub label: String,
    /// Seconds-since-epoch. Lets the test pick a fixed cutoff
    /// without leaning on `chrono::Utc::now()`.
    pub created_unix: i64,
}

impl Prunable for AuditEntry {
    fn prune_queryset() -> QuerySet<Self> {
        // Anything with `created_unix < 1000` is "stale" — picked
        // small so the seed below straddles it.
        QuerySet::<Self>::default().filter("created_unix__lt", 1000_i64)
    }
}
rustango::register_prunable!(AuditEntry);

#[derive(Model, Debug, Clone)]
#[rustango(table = "prl_session")]
#[allow(dead_code)]
pub struct StaleSession {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub token: String,
    pub expires_unix: i64,
}

impl Prunable for StaleSession {
    fn prune_queryset() -> QuerySet<Self> {
        // Anything with `expires_unix < 500` is expired.
        QuerySet::<Self>::default().filter("expires_unix__lt", 500_i64)
    }
}
rustango::register_prunable!(StaleSession);

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE prl_audit (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            label        TEXT NOT NULL,
            created_unix INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE prl_session (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            token        TEXT NOT NULL,
            expires_unix INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    // 2 stale audits + 2 fresh
    for (label, t) in [
        ("stale-a", 100),
        ("stale-b", 200),
        ("fresh-a", 1500),
        ("fresh-b", 9000),
    ] {
        let mut e = AuditEntry {
            id: Auto::default(),
            label: label.into(),
            created_unix: t,
        };
        e.save_pool(pool).await.unwrap();
    }
    // 1 stale session + 2 alive
    for (token, e) in [("expired-1", 100), ("alive-1", 1000), ("alive-2", 5000)] {
        let mut s = StaleSession {
            id: Auto::default(),
            token: token.into(),
            expires_unix: e,
        };
        s.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn registered_names_includes_both_models() {
    let names = registered_names();
    assert!(
        names.contains(&"prl_audit"),
        "expected prl_audit in registry: {names:?}"
    );
    assert!(
        names.contains(&"prl_session"),
        "expected prl_session in registry: {names:?}"
    );
}

#[tokio::test]
async fn prune_pretend_counts_without_deleting() {
    let pool = make_pool().await;
    seed(&pool).await;

    let reports = prune_pretend(&pool, &PruneOptions::default())
        .await
        .unwrap();
    let audit = reports.iter().find(|r| r.table == "prl_audit").unwrap();
    let session = reports.iter().find(|r| r.table == "prl_session").unwrap();
    assert_eq!(audit.rows, 2, "2 stale audit rows match");
    assert_eq!(session.rows, 1, "1 stale session matches");

    // Nothing was deleted.
    let audits: Vec<AuditEntry> = QuerySet::<AuditEntry>::default()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(audits.len(), 4, "no rows actually deleted");
}

#[tokio::test]
async fn prune_all_deletes_matching_rows() {
    let pool = make_pool().await;
    seed(&pool).await;

    let reports = prune_all(&pool, &PruneOptions::default()).await.unwrap();
    let audit = reports.iter().find(|r| r.table == "prl_audit").unwrap();
    let session = reports.iter().find(|r| r.table == "prl_session").unwrap();
    assert_eq!(audit.rows, 2);
    assert_eq!(session.rows, 1);

    // Survivors only.
    let audits: Vec<AuditEntry> = QuerySet::<AuditEntry>::default()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(audits.len(), 2);
    for a in &audits {
        assert!(
            a.created_unix >= 1000,
            "stale row leaked through prune: {} @ {}",
            a.label,
            a.created_unix
        );
    }
    let sessions: Vec<StaleSession> = QuerySet::<StaleSession>::default()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
}

#[tokio::test]
async fn prune_only_restricts_to_listed_models() {
    let pool = make_pool().await;
    seed(&pool).await;

    let opts = PruneOptions {
        only: vec!["prl_audit".into()],
        ..PruneOptions::default()
    };
    let reports = prune_all(&pool, &opts).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].table, "prl_audit");

    // Session table untouched.
    let sessions: Vec<StaleSession> = QuerySet::<StaleSession>::default()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 3, "session prune was filtered out");
}

#[tokio::test]
async fn prune_except_skips_listed_models() {
    let pool = make_pool().await;
    seed(&pool).await;

    let opts = PruneOptions {
        except: vec!["prl_audit".into()],
        ..PruneOptions::default()
    };
    let reports = prune_all(&pool, &opts).await.unwrap();
    let names: Vec<&str> = reports.iter().map(|r| r.table.as_str()).collect();
    assert!(!names.contains(&"prl_audit"));
    assert!(names.contains(&"prl_session"));

    // Audit table untouched.
    let audits: Vec<AuditEntry> = QuerySet::<AuditEntry>::default()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(audits.len(), 4, "audit prune was filtered out");
}
