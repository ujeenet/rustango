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
//! None of these declare `id`. A serializer field must match its
//! model's field type, and an `Auto<i64>` primary key has no sensible
//! serializer spelling — declaring `pub id: i64` fails to compile with
//! `expected &i64, found &Auto<i64>` pointing at the derive. Every
//! serializer in the framework's own test suite omits the PK for the
//! same reason.

use rustango::Serializer;

use super::models::{Customer, Order, Product};

/// `description` → `blurb`.
///
/// Omit `blurb` from a POST and the error must name `blurb`.
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Product)]
#[allow(dead_code)]
pub struct ProductSerializer {
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
    #[serializer(source = "email")]
    pub contact_email: String,
    pub full_name: String,
    pub loyalty_tier: String,
}

/// `reference` → `ref_code`.
///
/// **This serializer carries no foreign-key column, and that is a
/// framework limitation rather than a choice.** A serializer field must
/// match its model field's type exactly, and `ForeignKey<T>` implements
/// none of `Deserialize`, `Default` or `OpenApiSchema` — so declaring
/// `customer_id` here, under any name and any type, does not compile.
/// The only foreign-key serializer in the framework's own tests uses
/// `#[serializer(nested)]`, which is read-oriented.
///
/// The consequence for this app: the nullable-FK write path (#1450)
/// cannot be exercised *through* a serializer, so `/api/v1/orders-raw`
/// exists to exercise it without one. See `urls.rs`.
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Order)]
#[allow(dead_code)]
pub struct OrderSerializer {
    #[serializer(source = "reference")]
    pub ref_code: String,
    pub note: Option<String>,
    pub status: String,
    pub total_cents: i64,
}
