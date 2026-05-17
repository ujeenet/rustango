//! Compile + type-check coverage for the composite-primary-key
//! pattern documented in `crate::composite_pk` (Issue #46).
//!
//! Pins:
//! 1. `#[rustango(unique_together = "...")]` accepts the composite
//!    column tuple and propagates into [`ModelSchema::indexes`] as a
//!    UNIQUE btree index.
//! 2. The `.where_(a.eq).where_(b.eq)` lookup chain type-checks
//!    against a real `#[derive(Model)]` type — the SQL it emits is
//!    the same composite-key seek Django's native
//!    `CompositePrimaryKey` lookup would generate.
//!
//! No live DB required — all assertions are Rust-side.

#![cfg(feature = "postgres")]

use rustango::core::{Column as _, Model as _, ModelSchema};
use rustango::query::QuerySet;
use rustango::sql::Auto;
use rustango::Model;

// ============================================================
// Canonical shape: tenant-scoped invoice (the textbook
// composite-PK use case — invoice numbers unique per tenant).
// ============================================================

#[derive(Model, Debug)]
#[rustango(table = "cpk_invoice")]
#[rustango(unique_together = "tenant_id, invoice_number")]
#[allow(dead_code)]
pub struct Invoice {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tenant_id: i64,
    #[rustango(max_length = 32)]
    pub invoice_number: String,
    #[rustango(max_length = 200)]
    pub customer: String,
}

// ============================================================
// Three-column variant: line items inside an invoice
// (tenant_id, invoice_id, line_no all together).
// ============================================================

#[derive(Model, Debug)]
#[rustango(table = "cpk_invoice_line")]
#[rustango(unique_together = "tenant_id, invoice_id, line_no")]
#[allow(dead_code)]
pub struct InvoiceLine {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tenant_id: i64,
    pub invoice_id: i64,
    pub line_no: i32,
    #[rustango(max_length = 64)]
    pub sku: String,
}

// ============================================================

#[test]
fn unique_together_propagates_into_schema_as_composite_unique_index() {
    // Pin: the macro registers an `IndexSchema { unique: true,
    // columns: &[...] }` on the schema — this is the row Django's
    // `CompositePrimaryKey` would create implicitly, and the row the
    // DDL emitter turns into `CREATE UNIQUE INDEX ... (tenant_id,
    // invoice_number)`.
    let schema: &ModelSchema = Invoice::SCHEMA;
    let composite_uniques: Vec<_> = schema
        .indexes
        .iter()
        .filter(|i| i.unique && i.columns.len() >= 2)
        .collect();
    assert!(
        !composite_uniques.is_empty(),
        "Invoice::SCHEMA should carry at least one composite UNIQUE index"
    );
    let cols: Vec<&str> = composite_uniques[0].columns.iter().copied().collect();
    assert_eq!(cols, vec!["tenant_id", "invoice_number"]);
}

#[test]
fn three_column_unique_together_round_trips() {
    let schema: &ModelSchema = InvoiceLine::SCHEMA;
    let three_col: Vec<_> = schema
        .indexes
        .iter()
        .filter(|i| i.unique && i.columns.len() == 3)
        .collect();
    assert_eq!(
        three_col.len(),
        1,
        "InvoiceLine should have exactly one 3-column UNIQUE index"
    );
    let cols: Vec<&str> = three_col[0].columns.iter().copied().collect();
    assert_eq!(cols, vec!["tenant_id", "invoice_id", "line_no"]);
}

#[test]
fn composite_key_lookup_chains_type_check_against_querysets() {
    // Pin: the documented `.where_(a.eq).where_(b.eq)` chain — the
    // direct stand-in for Django's `.get(pk=(7, "INV-0001"))` —
    // produces a usable `QuerySet<T>` against a real Model derive.
    let _by_composite: QuerySet<Invoice> = Invoice::objects()
        .where_(Invoice::tenant_id.eq(7))
        .where_(Invoice::invoice_number.eq("INV-0001"));

    // And the three-column variant chains the same way.
    let _by_three: QuerySet<InvoiceLine> = InvoiceLine::objects()
        .where_(InvoiceLine::tenant_id.eq(7))
        .where_(InvoiceLine::invoice_id.eq(42))
        .where_(InvoiceLine::line_no.eq(1));
}

#[test]
fn surrogate_id_is_still_the_framework_pk() {
    // Pin: even though `(tenant_id, invoice_number)` is the *logical*
    // composite key, `Auto<i64>` remains the framework PK — so admin
    // row links, FK targets, and `save()` all keep working unchanged.
    // The composite-PK pattern is purely additive on top of that.
    let schema: &ModelSchema = Invoice::SCHEMA;
    let pk_field = schema
        .fields
        .iter()
        .find(|f| f.primary_key)
        .expect("Invoice should have a primary-key field");
    assert_eq!(pk_field.name, "id");
    assert!(
        pk_field.auto,
        "the surrogate PK is the framework's Auto<i64> column"
    );
}
