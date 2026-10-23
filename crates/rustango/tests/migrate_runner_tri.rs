//! The migrate runner's lock, its refusals, generated names and the FKs a
//! key change must move, on every backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::{tri_dialect_test, Model};

async fn no_setup(_pool: &Pool) {}

/// Two composite indexes whose derived names share their first 63 bytes,
/// and a field index whose derived name is longer than MySQL's 64.
#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_subscription_notification_preference_entries")]
#[rustango(index("primary_contact_email_address, secondary_a"))]
#[rustango(index("primary_contact_email_address, secondary_b"))]
#[allow(dead_code)]
pub struct LongIdx {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(index, max_length = 40)]
    pub primary_contact_email_address: String,
    #[rustango(max_length = 10)]
    pub secondary_a: String,
    #[rustango(max_length = 10)]
    pub secondary_b: String,
}

#[test]
fn derived_index_names_fit_and_differ() {
    let names: Vec<&str> = LongIdx::SCHEMA.indexes.iter().map(|i| i.name).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    for n in &names {
        assert!(n.len() <= 63, "`{n}` is {} bytes", n.len());
    }
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{names:?}");
}

/// The model's indexes create on every backend.
async fn long_index_names_apply(pool: &Pool) {
    fresh_table::<LongIdx>(pool).await;
    drop_table(pool, LongIdx::SCHEMA.table).await;
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [
        long_index_names_apply,
    ]
);
