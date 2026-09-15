//! App-level tests that need no database.
//!
//! The ones that do need a database live in `tests/`, where they can
//! skip cleanly when `DATABASE_URL` is unset.

/// Every `#[derive(Model)]` registers itself in `inventory` at link
/// time, and the auto-admin walks that registry — so a model missing
/// here is a model the admin will not show.
///
/// Listing all seven rather than one: this is the cheapest place to
/// notice a model that was written but never reached, which is a
/// failure mode no HTTP test can see.
#[test]
fn every_commerce_model_is_registered() {
    use rustango::core::ModelEntry;
    let tables: Vec<&'static str> = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema.table)
        .collect();

    for expected in [
        "commerce_customer",
        "commerce_staff",
        "commerce_product",
        "commerce_inventory",
        "commerce_order",
        "commerce_order_line",
        "commerce_shipment_event",
    ] {
        assert!(
            tables.iter().any(|t| *t == expected),
            "`{expected}` missing from inventory; tables: {tables:?}",
        );
    }
}

/// The #1450 column has to stay nullable, because a nullable `bigint`
/// is the whole reason it exists. Drop the `Option` and the test that
/// matters stops testing anything — silently, since a non-null insert
/// works fine on every dialect.
#[test]
fn the_picker_column_is_still_nullable() {
    use rustango::core::ModelEntry;
    let order = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == "commerce_order")
        .expect("commerce_order registered");

    let picker = order
        .schema
        .fields
        .iter()
        .find(|f| f.column == "assigned_picker_id")
        .expect("commerce_order has an assigned_picker_id column");

    assert!(
        picker.nullable,
        "assigned_picker_id must stay nullable — it is the #1450 guard, and a \
         NOT NULL column never reaches the binder that was broken"
    );

    // And the control alongside it: a nullable TEXT column, which
    // always worked. If both were non-nullable the pair proves nothing.
    let note = order
        .schema
        .fields
        .iter()
        .find(|f| f.column == "note")
        .expect("commerce_order has a note column");
    assert!(note.nullable, "note is the nullable-TEXT control");
}

/// The supervisor has to retire queues, not only start them.
///
/// It only ever added: `ensure_queue` for every active tenant, tick
/// after tick, and nothing for a tenant that stopped being active. The
/// queue kept two workers polling a database the tenant no longer used,
/// held its pool against the server's connection limit and against the
/// 64-pool cache cap (which has no eviction), and `shutdown_all` drained
/// it on every deploy.
///
/// Nothing reported it, because a deactivated tenant produces no error —
/// it just stops appearing in the registry query. The set difference
/// below *is* the missing signal, so it is what this pins.
#[test]
fn a_deactivated_tenant_is_retired() {
    use crate::commerce::supervisor::retired_slugs;

    let running = vec!["acme".to_string(), "globex".to_string(), "初芝".to_string()];

    // Steady state: every running queue still active, nothing retired.
    assert!(
        retired_slugs(&running, &running).is_empty(),
        "a tick that changes nothing must retire nothing"
    );

    // `globex` deactivated (or deleted — both look the same from here).
    let active = vec!["acme".to_string(), "初芝".to_string()];
    assert_eq!(
        retired_slugs(&running, &active),
        vec!["globex".to_string()],
        "a tenant absent from the active list must be retired"
    );

    // A tenant that is active but has no queue yet is `ensure_queue`'s
    // job, not this one — it must not appear here.
    let active_plus_new = vec![
        "acme".to_string(),
        "globex".to_string(),
        "初芝".to_string(),
        "newco".to_string(),
    ];
    assert!(
        retired_slugs(&running, &active_plus_new).is_empty(),
        "a newly provisioned tenant is not a retirement"
    );

    // Every tenant gone — a registry emptied by a bad migration, say.
    assert_eq!(
        retired_slugs(&running, &[]).len(),
        3,
        "all queues retire when no tenant is active"
    );
}
