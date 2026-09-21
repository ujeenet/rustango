//! A serializer must be able to carry a foreign-key column (#1454).
//!
//! It could not — under any name, with or without `source`. A
//! serializer field must match its model field's type exactly, and
//! `ForeignKey<T>` implemented none of the traits the derive requires:
//!
//! * `pub customer_id: i64` — the spelling most people reach for
//!   first — failed with `expected &i64, found &ForeignKey<Customer>`;
//! * `pub customer_id: ForeignKey<Customer>` failed on three missing
//!   bounds at once: `DeserializeOwned`, `Default` and `OpenApiSchema`.
//!
//! So every model with a relation — which is most of them — had to give
//! up field renaming, per-field validation, `read_only` and OpenAPI
//! generation on the related column, or drop the serializer entirely.
//! In the commerce soak that meant a second, serializer-less ViewSet
//! existed purely so a nullable FK could be written at all.
//!
//! `#[serializer(nested)]` was the only foreign-key serializer in the
//! test suite, and it renders a nested object on read — it is not a
//! writable scalar key.

#![cfg(all(feature = "serializer", feature = "sqlite"))]

use rustango::serializer::ModelSerializer;
use rustango::sql::{Auto, ForeignKey};
use rustango::{Model, Serializer};

#[derive(Model, Debug, Clone)]
#[rustango(table = "fk_ser_customer", display = "email")]
#[allow(dead_code)]
pub struct Customer {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub email: String,
}

/// A second target, only so the two foreign keys below point at
/// different models. Two FKs to the *same* model collide on the
/// derive's generated accessor name (`<target>_set_pool`) — a separate,
/// pre-existing limitation that has nothing to do with #1454.
#[derive(Model, Debug, Clone)]
#[rustango(table = "fk_ser_referrer", display = "code")]
#[allow(dead_code)]
pub struct Referrer {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fk_ser_order", display = "reference")]
#[allow(dead_code)]
pub struct Order {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub reference: String,
    /// The column that could not be serialised.
    pub customer_id: ForeignKey<Customer, i64>,
    /// And the nullable case — the shape #1450 was about, which had to
    /// be written through a serializer-less ViewSet because of this.
    pub referred_by_id: Option<ForeignKey<Referrer, i64>>,
}

/// The whole point: this declaration did not compile before #1454.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Order)]
#[allow(dead_code)]
pub struct OrderSerializer {
    #[serializer(source = "reference")]
    pub ref_code: String,
    pub customer_id: ForeignKey<Customer, i64>,
    pub referred_by_id: Option<ForeignKey<Referrer, i64>>,
}

fn order() -> Order {
    Order {
        id: Auto::Set(1),
        reference: "ORD-1".into(),
        customer_id: ForeignKey::unloaded(42),
        referred_by_id: Some(ForeignKey::unloaded(7)),
    }
}

#[test]
fn a_foreign_key_column_reaches_the_serializer() {
    let s = OrderSerializer::from_model(&order());
    assert_eq!(s.ref_code, "ORD-1", "the source rename still works");
    assert_eq!(*s.customer_id.pk_ref(), 42);
    assert_eq!(s.referred_by_id.as_ref().map(|f| *f.pk_ref()), Some(7));
}

/// It must render as the **key**, not as an object — that is what a
/// REST client sends back.
#[test]
fn it_renders_as_the_key() {
    let s = OrderSerializer::from_model(&order());
    let json = serde_json::to_value(&s).expect("serialize");
    assert_eq!(json["customer_id"], serde_json::json!(42));
    assert_eq!(json["referred_by_id"], serde_json::json!(7));
    assert!(
        !json["customer_id"].is_object(),
        "a foreign key must serialise as its key, not as a nested object: {json}"
    );
}

/// And read back. Without `Deserialize` the write path could not accept
/// the field at all, which is what made the column unusable.
#[test]
fn it_round_trips_through_json() {
    let parsed: OrderSerializer = serde_json::from_value(serde_json::json!({
        "ref_code": "ORD-9",
        "customer_id": 99,
        "referred_by_id": null,
    }))
    .expect("deserialize");

    assert_eq!(parsed.ref_code, "ORD-9");
    assert_eq!(*parsed.customer_id.pk_ref(), 99);
    assert!(
        parsed.referred_by_id.is_none(),
        "a null foreign key must deserialise to None, not to key 0"
    );
}

/// The FK column is **writable**: that is the difference between this
/// and `#[serializer(nested)]`, which is read-only by construction.
#[test]
fn the_foreign_key_is_writable() {
    let writable = OrderSerializer::writable_fields();
    assert!(
        writable.contains(&"customer_id"),
        "a foreign-key field must be writable — a read-only one is what \
         `nested` already offered. Got: {writable:?}"
    );
}

/// `Default` exists because the derive needs it, and it is a
/// placeholder rather than a valid reference. Worth pinning: a future
/// change that made it mean something else would let a row be written
/// with a key pointing at nothing.
#[test]
fn default_is_a_placeholder_key_not_a_reference() {
    let d: ForeignKey<Customer, i64> = ForeignKey::default();
    assert_eq!(*d.pk_ref(), 0);
    assert!(!d.is_loaded());
}
