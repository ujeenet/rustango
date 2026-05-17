//! Composite-primary-key patterns — Django 5.2's `CompositePrimaryKey`
//! adapted to rustango idioms. Issue #46.
//!
//! Django 5.2 shipped first-class composite-PK support:
//!
//! ```python
//! class OrderLine(models.Model):
//!     pk = models.CompositePrimaryKey("order_id", "line_no")
//!     order_id = models.BigIntegerField()
//!     line_no = models.IntegerField()
//!     sku = models.CharField(max_length=64)
//! ```
//!
//! rustango is single-`Auto<i64>`-PK only at the ORM layer today;
//! native `#[rustango(primary_key = ("a", "b"))]` is a v0.48 large-lift
//! item (touches `Model` trait, FK target resolution, admin pk-parser,
//! `inspectdb`, and tri-dialect DDL emitters).
//!
//! **You don't have to wait for v0.48** — the canonical Django-pre-5.2
//! pattern (which Django itself shipped for a decade before
//! `CompositePrimaryKey` landed) maps directly to rustango today and
//! covers every real-world composite-key use case: invoices with line
//! numbers, tenant-scoped resources, M2M through-tables, audit logs.
//!
//! ## The pattern: surrogate `Auto<i64>` + `unique_together`
//!
//! 1. Keep an `Auto<i64>` surrogate PK on every model (what the
//!    framework needs for `save()`, FK targets, admin row links).
//! 2. Declare the *logical* composite uniqueness with
//!    `#[rustango(unique_together = "a, b")]` — shipped since v0.19.
//! 3. Look rows up by the composite via a `.where_(a.eq).where_(b.eq)`
//!    chain — the underlying UNIQUE INDEX makes it index-equivalent to
//!    a primary-key lookup.
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
//! The schema this produces is *identical* in shape to a real
//! composite PK except for the extra `id BIGINT` column — a `UNIQUE`
//! index on `(order_id, line_no)` enforces the same invariant the
//! composite PK would.
//!
//! ## Tenant-scoped composite keys
//!
//! The single most common composite-PK shape — "this row is unique
//! *within this tenant*" — fits the pattern exactly:
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
//! Two different tenants can each have `invoice_number = "INV-0001"`,
//! same as Django's `CompositePrimaryKey("tenant_id", "invoice_number")`
//! would allow.
//!
//! ## Looking up by the composite key
//!
//! The query the framework would auto-generate for a `CompositePrimaryKey`
//! lookup is exactly what you write today:
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
//! Because the `(tenant_id, invoice_number)` UNIQUE INDEX is present,
//! Postgres / MySQL / SQLite all serve this as a single index seek —
//! same physical access path as a true composite PK.
//!
//! ## When to wait for v0.48 native support
//!
//! Three cases the surrogate-id workaround doesn't fully cover:
//!
//! 1. **Composite FK targets.** If a downstream model needs an FK
//!    *whose target is the composite*, you have to choose one column
//!    as the FK column today (or carry both columns + a denormalized
//!    `parent_id` surrogate). Native composite PK lets you reference
//!    `(order_id, line_no)` as a real FK pair.
//! 2. **`inspectdb` round-trip against legacy composite-PK tables.**
//!    rustango will emit a warning and pick one column; you have to
//!    add the surrogate column by hand to the generated struct.
//! 3. **Strict Django parity in ORM lookups.** Django auto-builds the
//!    composite WHERE on `.get(pk=(7, "INV-0001"))`. You write the two
//!    `.filter()` calls instead — same SQL, three more keystrokes.
//!
//! When any of these blocks you, file a `+1` on issue #46 — the
//! priority list is driven by demand.
//!
//! ## Summary
//!
//! | Django shape | rustango idiom | Status |
//! |---|---|---|
//! | `CompositePrimaryKey("a", "b")` | `Auto<i64>` + `unique_together = "a, b"` | shipped (v0.19) |
//! | `.get(pk=(7, "x"))` | `.where_(a.eq(7)).where_(b.eq("x"))` | shipped |
//! | composite FK target | denormalized parent surrogate column | v0.48 native |
//! | `inspectdb` composite round-trip | manual surrogate-column add | v0.48 native |
//!
//! See `tests/composite_pk_pattern.rs` for compile + behavior pins
//! against real `#[derive(Model)]` types.

// Doc-only module; the pattern this documents is exercised in
// `tests/composite_pk_pattern.rs`.
