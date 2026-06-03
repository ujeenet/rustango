//! Django parity — `Meta.constraints = [ExclusionConstraint(...)]`
//! lets a model declare Postgres `EXCLUDE USING …` constraints
//! (no two rows of group X may overlap in column Y). rustango spells
//! the attribute as `#[rustango(exclude(name = "…", using = "gist",
//! elements = "col WITH op, col WITH op", where = "…"))]`. PG-only:
//! the migration writer skips emission on MySQL/SQLite (with a
//! warning) so cross-dialect projects can keep one model declaration.

#![cfg(feature = "postgres")]

use rustango::migrate::diff::{detect_changes, SchemaChange};
use rustango::migrate::snapshot::{ExclusionSnapshot, SchemaSnapshot};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "exc_booking",
    exclude(
        name = "exc_booking_no_overlap",
        using = "gist",
        elements = "room_id WITH =, during WITH &&",
    ),
    exclude(
        name = "exc_booking_active_only",
        elements = "room_id WITH =",
        where = "cancelled_at IS NULL",
    )
)]
#[allow(dead_code)]
pub struct Booking {
    #[rustango(primary_key)]
    pub id: i64,
    pub room_id: i64,
    // The actual range column would be `tstzrange` — we just need the
    // schema entry for the macro to accept the model; the live DDL
    // emission is checked through the snapshot, not by running it.
    pub during: String,
    pub cancelled_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "exc_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn schema_carries_exclusion_constraint_entries() {
    let schema = <Booking as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.exclusion_constraints.len(), 2);

    let no_overlap = schema
        .exclusion_constraints
        .iter()
        .find(|x| x.name == "exc_booking_no_overlap")
        .expect("missing exc_booking_no_overlap");
    assert_eq!(no_overlap.using, "gist");
    assert_eq!(no_overlap.elements, &[("room_id", "="), ("during", "&&")],);
    assert!(no_overlap.where_clause.is_none());

    let active = schema
        .exclusion_constraints
        .iter()
        .find(|x| x.name == "exc_booking_active_only")
        .expect("missing exc_booking_active_only");
    // `using` defaulted to `gist` when omitted.
    assert_eq!(active.using, "gist");
    assert_eq!(active.elements, &[("room_id", "=")]);
    assert_eq!(active.where_clause, Some("cancelled_at IS NULL"));

    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(plain.exclusion_constraints.is_empty());
}

#[test]
fn snapshot_round_trips_exclusion_constraint() {
    let schema = <Booking as rustango::core::Model>::SCHEMA;
    let snap = SchemaSnapshot::from_models(&[schema]);
    assert_eq!(snap.excludes.len(), 2);

    let by_name: std::collections::HashMap<&str, &ExclusionSnapshot> =
        snap.excludes.iter().map(|x| (x.name.as_str(), x)).collect();
    let overlap = by_name["exc_booking_no_overlap"];
    assert_eq!(overlap.table, "exc_booking");
    assert_eq!(overlap.using, "gist");
    assert_eq!(
        overlap.elements,
        vec![
            ("room_id".to_owned(), "=".to_owned()),
            ("during".to_owned(), "&&".to_owned()),
        ],
    );
    assert!(overlap.where_clause.is_none());

    let active = by_name["exc_booking_active_only"];
    assert_eq!(active.where_clause.as_deref(), Some("cancelled_at IS NULL"));
}

#[test]
fn diff_emits_add_exclusion_for_new_constraint() {
    let schema = <Booking as rustango::core::Model>::SCHEMA;
    let prev = SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
        excludes: vec![],
    };
    let current = SchemaSnapshot::from_models(&[schema]);
    let changes = detect_changes(&prev, &current);

    let added: Vec<&SchemaChange> = changes
        .iter()
        .filter(|c| matches!(c, SchemaChange::AddExclusionConstraint { .. }))
        .collect();
    assert_eq!(added.len(), 2, "expected 2 AddExclusionConstraint ops");

    assert!(added.iter().any(|c| matches!(
        c,
        SchemaChange::AddExclusionConstraint { name, using, .. }
            if name == "exc_booking_no_overlap" && using == "gist"
    )));
    assert!(added.iter().any(|c| matches!(
        c,
        SchemaChange::AddExclusionConstraint { name, where_clause: Some(w), .. }
            if name == "exc_booking_active_only" && w == "cancelled_at IS NULL"
    )));
}

#[test]
fn diff_emits_drop_exclusion_when_constraint_removed() {
    let schema = <Booking as rustango::core::Model>::SCHEMA;
    let prev = SchemaSnapshot::from_models(&[schema]);
    // Current has the same table but the snapshot strips the excludes
    // (simulating the operator removing the `exclude(...)` attr).
    let mut current = SchemaSnapshot::from_models(&[schema]);
    current.excludes.clear();
    let changes = detect_changes(&prev, &current);

    let dropped: Vec<&SchemaChange> = changes
        .iter()
        .filter(|c| matches!(c, SchemaChange::DropExclusionConstraint { .. }))
        .collect();
    assert_eq!(dropped.len(), 2, "expected 2 DropExclusionConstraint ops");
}
