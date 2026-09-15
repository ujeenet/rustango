//! Serializers — and specifically, the `source` renames.
//!
//! **Twin file.** Identical in `platform_commerce_saas`.
//!
//! A `source` rename publishes a field under one name while the column
//! keeps another. Before 0.57.5 a validation error on such a field
//! reported the *column* name, so a client was told to fix a field the
//! API had never shown it (#1386). Every rename here exists to be
//! provoked from the API tests.
//!
//! Each declares `id` as `Auto<i64>` with `read_only` — a serializer
//! field must match its model field's type exactly, so `pub id: i64`
//! over an `Auto<i64>` primary key fails to compile with `expected
//! &i64, found &Auto<i64>` pointing at the derive. `read_only` keeps it
//! out of the writable set.
//!
//! Leaving it out compiles fine and is worse: the API then returns
//! created rows with no identifier, so a client cannot address what it
//! just made. That is how the soak driver first failed here.

use rustango::sql::{Auto, ForeignKey};
use rustango::Serializer;

use super::models::{Customer, Order, Product, StaffMember};

/// `description` → `blurb`.
///
/// Omit `blurb` from a POST and the error must name `blurb`.
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Product)]
#[allow(dead_code)]
pub struct ProductSerializer {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    pub sku: String,
    pub name: String,
    #[serializer(source = "description")]
    pub blurb: Option<String>,
    pub price_cents: i64,
    pub active: bool,
}

/// `email` → `contact_email`.
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Customer)]
#[allow(dead_code)]
pub struct CustomerSerializer {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    #[serializer(source = "email")]
    pub contact_email: String,
    pub full_name: String,
    pub loyalty_tier: String,
}

/// `reference` → `ref_code`, and the foreign keys.
///
/// The FK fields could not be declared here at all until #1454:
/// `ForeignKey<T>` implemented none of `Deserialize`, `Default` or
/// `OpenApiSchema`, and a serializer field must match its model field's
/// type exactly — so there was no spelling that compiled, and the
/// nullable-FK write path (#1450) could only be reached through a
/// serializer-less ViewSet. They now round-trip as their key, which is
/// what a REST client sends.
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Order)]
#[allow(dead_code)]
pub struct OrderSerializer {
    #[serializer(read_only)]
    pub id: Auto<i64>,
    #[serializer(source = "reference")]
    pub ref_code: String,
    pub customer_id: ForeignKey<Customer, i64>,
    /// The #1450 column, now reachable through a serializer (#1454).
    #[serializer(source = "assigned_picker_id")]
    pub picker_id: Option<ForeignKey<StaffMember, i64>>,
    pub note: Option<String>,
    pub status: String,
    pub total_cents: i64,
}
