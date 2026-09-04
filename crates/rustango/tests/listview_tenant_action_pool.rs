//! Issue #560 — `ListView::tenant_action_pool` builder.
//! Before this fix `ListView::tenant_action` only accepted a
//! PG-only `&mut PgConnection` handler, gated behind
//! `#[cfg(feature = "postgres")]`, so MySQL / SQLite tenants
//! couldn't register a per-request bulk action at all.
//!
//! This test verifies the new builder is callable on a non-PG
//! build and chains correctly with [`ListView::bulk_actions`].
//! The runtime dispatch path is exercised by the framework's
//! integration suite — here we just nail down the API.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "template_views"))]

use std::sync::Arc;

use rustango::core::SqlValue;
use rustango::sql::Pool;
use rustango::template_views::{BulkActionFuture, ListView, TenantBulkActionPoolFn};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "lvt_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    pub title: String,
}

fn make_handler() -> TenantBulkActionPoolFn {
    Arc::new(|_pool: &Pool, _pks: &[SqlValue]| -> BulkActionFuture<'_> {
        Box::pin(async move { Ok(()) })
    })
}

#[test]
fn tenant_action_pool_builder_compiles_on_non_pg_sqlite_build() {
    // The whole point of the fix — this code compiles on a
    // `--features sqlite,tenancy,template_views` build (no PG).
    let _lv = ListView::for_model(<Post as rustango::core::Model>::SCHEMA)
        .bulk_actions(true)
        .tenant_action_pool("mark_published", "Mark Published", make_handler());
}

#[test]
fn tenant_action_pool_chains_with_other_builders() {
    // Composes with the other ListView builders.
    let _lv = ListView::for_model(<Post as rustango::core::Model>::SCHEMA)
        .bulk_actions(true)
        .tenant_action_pool("publish", "Publish", make_handler())
        .tenant_action_pool("unpublish", "Unpublish", make_handler());
}
