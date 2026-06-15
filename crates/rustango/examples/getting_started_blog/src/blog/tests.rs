//! App-level integration tests.
//!
//! Run with `cargo test`. Uses `rustango::test_client::TestClient` to
//! exercise the app's router in-process — no network, no real socket.

use super::urls::api;

/// Smoke test — the empty router builds without panicking.
/// Replace with real route assertions once you add `.route(...)`
/// lines in `urls.rs`.
#[tokio::test]
async fn router_builds() {
    let _router = api();
}

/// Smoke test — every `#[derive(Model)]` in `models.rs` registers
/// itself in `inventory` at link time. The auto-admin walks that
/// registry, so seeing your model here is the canonical
/// confirmation that the admin will pick it up.
///
/// If you rename the starter model's `table = "..."`, update
/// the literal below.
#[test]
fn starter_model_registered_in_inventory() {
    use rustango::core::ModelEntry;
    let tables: Vec<&'static str> = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema.table)
        .collect();
    assert!(
        tables.iter().any(|t| *t == "posts"),
        "`posts` missing from inventory; tables: {tables:?}",
    );
}
