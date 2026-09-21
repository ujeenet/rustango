//! How to model a composite primary key, the way Django 5.2's
//! `CompositePrimaryKey` does.
//!
//! Django spells it like this:
//!
//! ```python
//! class OrderLine(models.Model):
//!     pk = models.CompositePrimaryKey("order_id", "line_no")
//!     order_id = models.BigIntegerField()
//!     line_no = models.IntegerField()
//!     sku = models.CharField(max_length=64)
//! ```
//!
//! The rustango ORM supports one `Auto<i64>` primary key per model.
//! Use the pattern Django itself used before 5.2. It covers the usual
//! cases: invoice line numbers, tenant-scoped rows, through-tables.
//!
//! ## The pattern: a surrogate key plus `unique_together`
//!
//! 1. Keep an `Auto<i64>` primary key. `save()`, FK targets and admin
//!    row links all need one.
//! 2. Declare the real uniqueness with
//!    `#[rustango(unique_together = "a, b")]`.
//! 3. Look rows up with `.where_(a.eq(..)).where_(b.eq(..))`. The
//!    UNIQUE index makes that as fast as a primary-key lookup.
//!
//! ```ignore
//! use rustango::sql::Auto;
//!
//! #[derive(rustango::Model, Debug)]
//! #[rustango(table = "order_line")]
//! #[rustango(unique_together = "order_id, line_no")]
//! pub struct OrderLine {
//!     #[rustango(primary_key)]
//!     pub id: Auto<i64>,
//!     pub order_id: i64,
//!     pub line_no: i32,
//!     #[rustango(max_length = 64)]
//!     pub sku: String,
//! }
//! ```
//!
//! The only difference from a real composite PK is the extra `id`
//! column. The UNIQUE index enforces the same rule.
//!
//! ## Tenant-scoped keys
//!
//! "Unique within one tenant" is the most common case:
//!
//! ```ignore
//! #[derive(rustango::Model, Debug)]
//! #[rustango(table = "invoice")]
//! #[rustango(unique_together = "tenant_id, invoice_number")]
//! pub struct Invoice {
//!     #[rustango(primary_key)]
//!     pub id: Auto<i64>,
//!     pub tenant_id: i64,
//!     #[rustango(max_length = 32)]
//!     pub invoice_number: String,
//!     #[rustango(max_length = 200)]
//!     pub customer: String,
//! }
//! ```
//!
//! Two tenants can each have `invoice_number = "INV-0001"`.
//!
//! ## Looking a row up
//!
//! ```ignore
//! use rustango::core::Column as _;
//! use rustango::query::QuerySet;
//!
//! let line = Invoice::objects()
//!     .where_(Invoice::tenant_id.eq(7))
//!     .where_(Invoice::invoice_number.eq("INV-0001"))
//!     .first_pool(&pool)
//!     .await?;
//! ```
//!
//! Postgres, MySQL and SQLite all serve this as one index seek.
//!
//! ## Two limits to know
//!
//! - **Foreign keys to the pair.** A child model cannot point at
//!   `(order_id, line_no)`. Point it at the surrogate `id` instead.
//! - **`inspectdb` on a legacy composite-PK table.** It warns, picks
//!   one column, and you add the surrogate column by hand.

// Doc-only module; the pattern is exercised in `tests/composite_pk_pattern.rs`.
