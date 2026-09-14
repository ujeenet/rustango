//! The commerce domain.
//!
//! **Twin file.** `platform_commerce_saas/src/commerce/models.rs` is
//! byte-identical to this one. No `scope = "tenant"` is needed: the
//! derive already defaults to tenant scope, so a model that says
//! nothing lands in the tenant's own storage. The two examples are
//! severed workspaces on purpose — each has to stay copy-pasteable out
//! of the repo — so the duplication is deliberate. Change one, change
//! the other.
//!
//! These models are not arbitrary. Each one carries at least one field
//! or constraint that exists to exercise a specific behaviour change
//! from the 0.57.x release, and the comments say which. A field with no
//! such note is there to make the app read like a shop rather than a
//! fixture.

use chrono::{DateTime, Utc};
use rustango::sql::{Auto, ForeignKey};
use rustango::Model;

/// Buyers.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "commerce_customer",
    display = "email",
    admin(
        list_display = "id, email, loyalty_tier, created_at",
        list_filter = "loyalty_tier",
        search_fields = "email"
    )
)]
pub struct Customer {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// `unique`, so a duplicate in a bulk POST rolls the whole batch
    /// back (#1403).
    #[rustango(max_length = 254, unique)]
    pub email: String,
    #[rustango(max_length = 120)]
    pub full_name: String,
    #[rustango(
        max_length = 16,
        default = "'bronze'",
        choices = "bronze:Bronze, silver:Silver, gold:Gold"
    )]
    pub loyalty_tier: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Staff who pick and pack orders.
///
/// This model exists to be the target of `Order::assigned_picker_id` —
/// a nullable foreign key is the whole point, and a FK needs somewhere
/// to point.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(table = "commerce_staff", display = "name")]
pub struct StaffMember {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    #[rustango(default = "true")]
    pub active: bool,
}

/// The catalogue.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "commerce_product",
    display = "sku",
    admin(
        list_display = "id, sku, name, price_cents, active",
        list_filter = "active",
        search_fields = "sku, name",
        ordering = "sku"
    )
)]
pub struct Product {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The bulk-create rollback trigger. A 50-item POST whose 30th row
    /// repeats an existing SKU must leave **zero** rows behind (#1403).
    #[rustango(max_length = 64, unique)]
    pub sku: String,
    #[rustango(max_length = 200)]
    pub name: String,
    /// Published as `blurb` by `ProductSerializer` — a `source` rename,
    /// so a validation error on this field must name `blurb` and not
    /// leak the column name (#1386).
    pub description: Option<String>,
    /// Integer cents. Floats are the wrong type for money and the
    /// showcase example makes the same choice.
    pub price_cents: i64,
    #[rustango(default = "true")]
    pub active: bool,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// Stock, per product per location.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "commerce_inventory",
    display = "location",
    unique_together = "product_id, location"
)]
pub struct InventoryItem {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub product_id: ForeignKey<Product, i64>,
    #[rustango(max_length = 32)]
    pub location: String,
    /// The reconciliation job and the `/api/v1/inventory` endpoint both
    /// write this column, deliberately: the soak wants that contention.
    pub on_hand: i64,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

/// Orders.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "commerce_order",
    display = "reference",
    admin(
        list_display = "id, reference, status, total_cents, placed_at",
        list_filter = "status",
        search_fields = "reference",
        ordering = "-placed_at"
    )
)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32, unique)]
    pub reference: String,
    pub customer_id: ForeignKey<Customer, i64>,

    /// **The #1450 guard, and the most load-bearing field in this file.**
    ///
    /// `Option<ForeignKey<..>>` is a nullable `bigint`. Before 0.57.5,
    /// `SqlValue::Null` bound with a *text* type OID, so PostgreSQL
    /// rejected every insert that left this unset:
    ///
    /// ```text
    /// column "assigned_picker_id" is of type bigint
    /// but expression is of type text
    /// ```
    ///
    /// Every order placed without a picker hits it on INSERT, and
    /// `PATCH {"assigned_picker_id": null}` hits the UPDATE binder,
    /// which is a separate call site with its own bug surface.
    ///
    /// Note this is a **Postgres-only** fix: the MySQL and SQLite
    /// binders were deliberately left unchanged, so those two arms are
    /// controls, not tests. A tri-dialect run that only asserts "no
    /// error" does not distinguish the fix.
    pub assigned_picker_id: Option<ForeignKey<StaffMember, i64>>,

    /// The control for the field above. A nullable `TEXT` column always
    /// worked, on every dialect. Having both in one row is what proves
    /// the fix was about the non-text binder rather than about NULL in
    /// general.
    pub note: Option<String>,

    #[rustango(
        max_length = 16,
        default = "'pending'",
        choices = "pending:Pending, paid:Paid, picking:Picking, shipped:Shipped, cancelled:Cancelled"
    )]
    pub status: String,
    pub total_cents: i64,
    #[rustango(auto_now_add)]
    pub placed_at: Auto<DateTime<Utc>>,
}

/// Line items.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "commerce_order_line",
    display = "id",
    unique_together = "order_id, product_id"
)]
pub struct OrderLine {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub order_id: ForeignKey<Order, i64>,
    pub product_id: ForeignKey<Product, i64>,
    /// The second bulk-rollback trigger: `unique_together` above means
    /// the same product twice in one array POST aborts the batch.
    pub quantity: i64,
    pub unit_price_cents: i64,
}

/// Append-only audit of what happened to an order.
///
/// Jobs write here, and `tenant_slug` is what makes the cross-tenant
/// leak check possible: every job stamps the tenant it believed it was
/// running for, so a sweep can assert no tenant's table names another.
/// In the single-tenant app the column is always `"__single__"`.
#[allow(dead_code)]
#[derive(Model, Debug, Clone)]
#[rustango(table = "commerce_shipment_event", display = "kind")]
pub struct ShipmentEvent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub order_id: ForeignKey<Order, i64>,
    #[rustango(max_length = 32)]
    pub kind: String,
    #[rustango(max_length = 64)]
    pub tenant_slug: String,
    pub detail: Option<String>,
    #[rustango(auto_now_add)]
    pub at: Auto<DateTime<Utc>>,
}
