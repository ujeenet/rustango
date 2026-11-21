//! Audit list / count / facets, a user's roles, media tag reads and the
//! notifications table insert behave the same on every backend.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "tenancy",
    feature = "media",
    feature = "notifications",
    feature = "testkit"
))]

use std::sync::Arc;

use rustango::audit::{self, AuditFilter, AuditLog, AuditOp, AuditSource, PendingEntry};
use rustango::media::{MediaManager, MediaTag, SaveOpts};
use rustango::notifications::{
    notify, Notifiable, Notification, NotificationContext, NotificationDispatch,
};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::storage::{InMemoryStorage, StorageRegistry};
use rustango::tenancy::permissions::{assign_role_pool, create_role_pool, user_roles_qs_pool};
use rustango::{tri_dialect_test, Model};

const T_A: &str = "ormb1_a";
const T_B: &str = "ormb1_b";
const T_C: &str = "ormb1_c";
const NOTIF_TABLE: &str = "ormb1_notifications";

/// The user-defined table the notifications channel writes to.
#[derive(Model, Debug)]
#[rustango(table = "ormb1_notifications")]
pub struct Notif {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub notifiable_id: i64,
    #[rustango(column = "type", max_length = 100)]
    pub kind: String,
    pub data: serde_json::Value,
}

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    rustango::testkit::matrix::fresh_table::<Notif>(pool).await;
    for t in [T_A, T_B, T_C] {
        AuditLog::delete_where("entity_table", t, pool)
            .await
            .expect("clear audit rows");
    }
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

async fn emit(pool: &Pool, table: &'static str, pk: &str, op: AuditOp, source: AuditSource) {
    audit::emit_one(
        pool,
        &PendingEntry {
            entity_table: table,
            entity_pk: pk.into(),
            operation: op,
            source,
            changes: serde_json::json!({ "pk": pk }),
        },
    )
    .await
    .expect("emit");
}

/// Four rows: `a` x2 (create, update), `b` x1 (create), `c` x1 (delete).
async fn seed_audit(pool: &Pool) {
    emit(pool, T_A, "1", AuditOp::Create, AuditSource::System).await;
    emit(pool, T_A, "1", AuditOp::Update, AuditSource::System).await;
    emit(pool, T_B, "7", AuditOp::Create, AuditSource::System).await;
    emit(pool, T_C, "9", AuditOp::Delete, AuditSource::System).await;
}

fn table_filter(t: &str) -> AuditFilter {
    AuditFilter {
        entity_table: Some(t.to_owned()),
        ..AuditFilter::default()
    }
}

/// Newest first, with a page and an offset, filtered by table and operation.
async fn audit_list_orders_pages_and_filters(pool: &Pool) {
    seed_audit(pool).await;
    let all = audit::list(pool, &table_filter(T_A), 10, 0).await.unwrap();
    let ops: Vec<&str> = all.iter().map(|e| e.operation.as_str()).collect();
    assert_eq!(ops, ["update", "create"], "newest first");
    assert_eq!(all[0].changes, serde_json::json!({ "pk": "1" }));

    let page2 = audit::list(pool, &table_filter(T_A), 1, 1).await.unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].operation, "create");

    let both = AuditFilter {
        entity_table: Some(T_A.to_owned()),
        operation: Some("create".to_owned()),
        ..AuditFilter::default()
    };
    assert_eq!(audit::list(pool, &both, 10, 0).await.unwrap().len(), 1);
    assert_eq!(audit::count(pool, &both).await.unwrap(), 1);
    assert_eq!(audit::count(pool, &table_filter(T_A)).await.unwrap(), 2);
}

/// Count descending, then value ascending; unknown columns are refused.
async fn audit_facets_count_and_order(pool: &Pool) {
    seed_audit(pool).await;
    let facets = audit::facet_counts(pool, "entity_table").await.unwrap();
    let mine: Vec<(&str, i64)> = facets
        .iter()
        .filter(|(v, _)| v.starts_with("ormb1_"))
        .map(|(v, n)| (v.as_str(), *n))
        .collect();
    assert_eq!(mine, [(T_A, 2), (T_B, 1), (T_C, 1)]);
    let ops = audit::facet_counts(pool, "operation").await.unwrap();
    assert!(ops
        .windows(2)
        .all(|w| w[0].1 > w[1].1 || (w[0].1 == w[1].1 && w[0].0 <= w[1].0)));
    assert!(matches!(
        audit::facet_counts(pool, "changes").await,
        Err(rustango::sql::sqlx::Error::ColumnNotFound(_))
    ));
}

async fn user_with_roles(pool: &Pool, names: &[&str]) -> i64 {
    let mut u = rustango::tenancy::User {
        username: unique("ormb1u"),
        ..rustango::testkit::user()
    };
    u.insert(pool).await.expect("user");
    let uid = *u.id.get().expect("pk");
    for n in names {
        let role = create_role_pool(&unique(n), "", pool).await.expect("role");
        assign_role_pool(uid, role, pool).await.expect("assign");
    }
    uid
}

/// Roles come back by name, and only the user's own.
async fn user_roles_are_the_users_own_by_name(pool: &Pool) {
    let uid = user_with_roles(pool, &["zeta", "alpha", "mid"]).await;
    let other = user_with_roles(pool, &["other"]).await;
    let roles = user_roles_qs_pool(uid, pool).await.unwrap();
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names.len(), 3);
    assert!(
        names[0].starts_with("alpha")
            && names[1].starts_with("mid")
            && names[2].starts_with("zeta")
    );
    assert_eq!(user_roles_qs_pool(other, pool).await.unwrap().len(), 1);
    assert!(user_roles_qs_pool(i64::MAX, pool).await.unwrap().is_empty());
}

fn manager(pool: &Pool) -> MediaManager {
    let registry = StorageRegistry::new()
        .set("default", Arc::new(InMemoryStorage::new()))
        .with_default("default");
    MediaManager::new(pool.clone(), registry)
}

/// `ensure_tag` creates once and is stable; tag reads and `untag` agree.
async fn media_tags_round_trip(pool: &Pool) {
    let mgr = manager(pool);
    let slug = unique("ormb1-tag-");
    let first = mgr.ensure_tag(&slug).await.unwrap();
    let again = mgr.ensure_tag(&slug).await.unwrap();
    assert_eq!(first.id.get(), again.id.get());
    assert_eq!(first.name, slug);
    let found = mgr.get_tag_by_slug(&slug).await.unwrap().expect("found");
    assert_eq!(found.slug, slug);
    assert!(mgr
        .get_tag_by_slug("ormb1-missing")
        .await
        .unwrap()
        .is_none());

    let m = mgr
        .save_bytes(SaveOpts {
            disk: "default".into(),
            key_prefix: "t/".into(),
            bytes: b"x".to_vec(),
            mime: "text/plain".into(),
            original_filename: "x.txt".into(),
            uploaded_by_id: None,
            collection_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("save");
    let id = *m.id.get().expect("pk");
    mgr.tag(id, &[slug.as_str(), "ormb1-other"]).await.unwrap();
    let tags: Vec<String> = mgr
        .tags_for(id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    assert_eq!(tags, ["ormb1-other".to_owned(), slug.clone()]);
    let listed = mgr.list_with_tag(&slug, 10, 0).await.unwrap();
    assert_eq!(listed.len(), 1);
    mgr.untag(id, &slug).await.unwrap();
    assert!(mgr.list_with_tag(&slug, 10, 0).await.unwrap().is_empty());
    assert_eq!(mgr.tags_for(id).await.unwrap().len(), 1);
    // The tag row itself stays.
    assert!(
        MediaTag::objects()
            .filter("slug", slug)
            .fetch(pool)
            .await
            .unwrap()
            .len()
            == 1
    );
}

struct Recipient;
impl Notifiable for Recipient {
    fn notification_id(&self) -> Option<i64> {
        Some(42)
    }
}
struct Ping;
impl Notification<Recipient> for Ping {
    fn build(&self, _: &Recipient) -> NotificationDispatch {
        NotificationDispatch {
            database: Some(serde_json::json!({ "hello": "world" })),
            ..NotificationDispatch::none()
        }
    }
}

/// The database channel writes `(notifiable_id, type, data)` into the configured table.
async fn database_channel_inserts_a_row(pool: &Pool) {
    let ctx = NotificationContext::new().with_database(pool.clone(), NOTIF_TABLE);
    let result = notify(&Recipient, &Ping, &ctx).await;
    assert!(result.no_failures(), "{result:?}");
    let rows = Notif::objects().fetch(pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].notifiable_id, 42);
    assert_eq!(rows[0].data, serde_json::json!({ "hello": "world" }));
    // A table that does not exist fails the channel, not the call.
    let bad = NotificationContext::new().with_database(pool.clone(), "ormb1_no_such_table");
    assert!(!notify(&Recipient, &Ping, &bad).await.no_failures());
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        audit_list_orders_pages_and_filters,
        audit_facets_count_and_order,
        user_roles_are_the_users_own_by_name,
        media_tags_round_trip,
        database_channel_inserts_a_row,
    ],
}
