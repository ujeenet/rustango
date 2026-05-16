//! PG `EXCLUDE` constraint migration primitive (issue #32). Adds
//! the `SchemaChange::AddExclusionConstraint` + `DropExclusionConstraint`
//! variants so users can encode booking-conflict-style constraints
//! in their migration files.
//!
//! Same ORM-extractability principle: the new variants live in
//! `migrate/diff.rs` (the SchemaChange IR) + `migrate/invert.rs`.
//! No admin / tenancy coupling.
//!
//! v1 scope: low-level migration primitive only. Users hand-write
//! the migration JSON entry — automatic detection from a model-side
//! `#[rustango(exclusion_constraint(...))]` attribute follows in a
//! separate macro slice.

use rustango::migrate::diff::render_changes_split_with_dialect;
use rustango::migrate::{SchemaChange, SchemaSnapshot};
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{MySql, Postgres};

fn render(change: SchemaChange, dialect: &dyn rustango::sql::Dialect) -> Vec<String> {
    let snap = SchemaSnapshot::default();
    let batch = render_changes_split_with_dialect(&[change], &snap, dialect).unwrap();
    let mut sql = batch.immediate;
    sql.extend(batch.deferred_fks);
    sql
}

// ---------- PG: native EXCLUDE emission ----------

#[test]
fn pg_emits_booking_conflict_exclusion_constraint() {
    let change = SchemaChange::AddExclusionConstraint {
        name: "bookings_no_conflict".into(),
        table: "bookings".into(),
        using: "gist".into(),
        elements: vec![
            ("room_id".into(), "=".into()),
            ("during".into(), "&&".into()),
        ],
        where_clause: None,
    };
    let sql = render(change, &Postgres).join("\n");
    assert!(
        sql.contains(
            r#"ALTER TABLE "bookings" ADD CONSTRAINT "bookings_no_conflict" EXCLUDE USING gist ("room_id" WITH =, "during" WITH &&)"#
        ),
        "PG EXCLUDE emission: {sql}",
    );
}

#[test]
fn pg_emits_exclusion_constraint_with_where_predicate() {
    let change = SchemaChange::AddExclusionConstraint {
        name: "active_bookings_no_conflict".into(),
        table: "bookings".into(),
        using: "gist".into(),
        elements: vec![
            ("room_id".into(), "=".into()),
            ("during".into(), "&&".into()),
        ],
        where_clause: Some("cancelled = false".into()),
    };
    let sql = render(change, &Postgres).join("\n");
    assert!(
        sql.ends_with(" WHERE (cancelled = false)"),
        "WHERE predicate appended: {sql}",
    );
    assert!(sql.contains("EXCLUDE USING gist"));
}

#[test]
fn pg_supports_alternative_using_method() {
    // `btree_gist` is the extension that lets EXCLUDE use btree
    // comparison operators alongside GiST's overlap operator —
    // useful when one element is `=` over an int column.
    let change = SchemaChange::AddExclusionConstraint {
        name: "rooms_no_overlap".into(),
        table: "rooms".into(),
        using: "btree_gist".into(),
        elements: vec![("name".into(), "=".into())],
        where_clause: None,
    };
    let sql = render(change, &Postgres).join("\n");
    assert!(
        sql.contains("EXCLUDE USING btree_gist"),
        "alternative method: {sql}",
    );
}

#[test]
fn pg_emits_drop_exclusion_constraint() {
    let change = SchemaChange::DropExclusionConstraint {
        name: "bookings_no_conflict".into(),
        table: "bookings".into(),
    };
    let sql = render(change, &Postgres).join("\n");
    assert!(
        sql.contains(r#"ALTER TABLE "bookings" DROP CONSTRAINT IF EXISTS "bookings_no_conflict""#),
        "PG DROP CONSTRAINT: {sql}",
    );
}

// ---------- MySQL / SQLite: silent skip + warning ----------

#[test]
fn mysql_skips_exclusion_constraint_silently() {
    let change = SchemaChange::AddExclusionConstraint {
        name: "n".into(),
        table: "t".into(),
        using: "gist".into(),
        elements: vec![("c".into(), "&&".into())],
        where_clause: None,
    };
    let sql = render(change, &MySql);
    // No SQL emitted — MySQL has no EXCLUDE constraint, so we skip
    // (logged via tracing::warn) rather than crash. The rest of the
    // migration still applies.
    assert!(sql.is_empty(), "MySQL: nothing emitted, got: {:?}", sql);
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_skips_exclusion_constraint_silently() {
    let change = SchemaChange::AddExclusionConstraint {
        name: "n".into(),
        table: "t".into(),
        using: "gist".into(),
        elements: vec![("c".into(), "&&".into())],
        where_clause: None,
    };
    let sql = render(change, &Sqlite);
    assert!(sql.is_empty());
}

#[test]
fn mysql_skips_drop_exclusion_constraint_silently() {
    let change = SchemaChange::DropExclusionConstraint {
        name: "n".into(),
        table: "t".into(),
    };
    let sql = render(change, &MySql);
    assert!(sql.is_empty());
}

// ---------- Serialization round-trip (forward-compat) ----------

#[test]
fn add_exclusion_round_trips_through_json() {
    let change = SchemaChange::AddExclusionConstraint {
        name: "n".into(),
        table: "t".into(),
        using: "gist".into(),
        elements: vec![("a".into(), "=".into()), ("b".into(), "&&".into())],
        where_clause: Some("active = true".into()),
    };
    let json = serde_json::to_string(&change).unwrap();
    let back: SchemaChange = serde_json::from_str(&json).unwrap();
    assert_eq!(back, change);
}

#[test]
fn add_exclusion_defaults_using_to_gist_when_absent() {
    // Older migration files (pre-#32) won't have the `using` key.
    // The serde `default = "default_exclusion_method"` should fill
    // it with "gist".
    let json = r#"{"AddExclusionConstraint":{"name":"n","table":"t","elements":[["a","="]]}}"#;
    let back: SchemaChange = serde_json::from_str(json).unwrap();
    match back {
        SchemaChange::AddExclusionConstraint {
            using,
            where_clause,
            ..
        } => {
            assert_eq!(using, "gist");
            assert!(where_clause.is_none());
        }
        other => panic!("expected AddExclusionConstraint, got {other:?}"),
    }
}

// ---------- Inversion ----------

#[test]
fn invert_add_exclusion_yields_drop() {
    use rustango::migrate::{invert, Operation, SchemaSnapshot};
    let add = Operation::Schema(SchemaChange::AddExclusionConstraint {
        name: "n".into(),
        table: "t".into(),
        using: "gist".into(),
        elements: vec![("c".into(), "&&".into())],
        where_clause: None,
    });
    let prev = SchemaSnapshot::default();
    let inverted = invert(&[add], &prev).unwrap();
    assert_eq!(inverted.len(), 1);
    assert!(matches!(
        inverted[0],
        Operation::Schema(SchemaChange::DropExclusionConstraint { ref name, .. }) if name == "n"
    ));
}

#[test]
fn invert_drop_exclusion_errors_with_clear_message() {
    use rustango::migrate::{invert, Operation, SchemaSnapshot};
    let drop = Operation::Schema(SchemaChange::DropExclusionConstraint {
        name: "n".into(),
        table: "t".into(),
    });
    let prev = SchemaSnapshot::default();
    let r = invert(&[drop], &prev);
    assert!(
        r.is_err(),
        "drop inversion should fail (no snapshot record)"
    );
    let msg = format!("{}", r.unwrap_err());
    assert!(
        msg.contains("exclusion") && msg.contains("hand"),
        "error should explain manual workaround: {msg}",
    );
}
