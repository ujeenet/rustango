//! Shop domain. Until #524 lands (macro `Decimal` support), prices
//! are stored as integer cents — common e-commerce pattern, avoids
//! float drift.

use rustango::sql::Auto;
use rustango::Model;

/// One product. `price_cents` exercises `i64` round-trip; `sku`
/// exercises `unique`; `stock` exercises `Option<i64>`; `active`
/// exercises bool + `default = "true"`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "showcase_shop_product", display = "name")]
pub struct Product {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 120)]
    pub name: String,

    /// SKU — useful as a filter target. Unique so a typo doesn't
    /// silently shadow an existing row.
    #[rustango(max_length = 32, unique)]
    pub sku: String,

    /// Price in cents (integer to avoid float drift). Track #524 for
    /// macro `Decimal` support that would let this be `Decimal`.
    pub price_cents: i64,

    /// Stock quantity. `Option<i64>` → nullable column so backorder
    /// shapes ("not tracked") differ from "0 in stock".
    pub stock: Option<i64>,

    /// Active flag — drives the `/shop/products?active=true` filter.
    #[rustango(default = "true")]
    pub active: bool,
}
