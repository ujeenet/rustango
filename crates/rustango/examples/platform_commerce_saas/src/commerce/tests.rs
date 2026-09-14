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
