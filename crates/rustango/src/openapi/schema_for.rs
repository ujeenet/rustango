//! `OpenApiSchema` trait + blanket impls for primitives, std types,
//! chrono date/time, uuid, and serde_json::Value.
//!
//! `#[derive(Serializer)]` emits `impl OpenApiSchema for YourSerializer`
//! when the `openapi` feature is on (forwarded through the macro crate).
//! That impl walks the struct fields and assembles a [`Schema::object`]
//! with each field's schema looked up via this trait. Primitive fields
//! "just work" because of the blanket impls below.
//!
//! ## Adding your own types
//!
//! Implement [`OpenApiSchema`] manually for any custom field type used
//! in a serializer:
//!
//! ```ignore
//! struct Money { cents: i64 }
//!
//! impl rustango::openapi::OpenApiSchema for Money {
//!     fn openapi_schema() -> rustango::openapi::Schema {
//!         rustango::openapi::Schema::integer()
//!             .description("amount in cents")
//!     }
//! }
//! ```

use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;

use super::Schema;

/// Map a Rust type to its OpenAPI / JSON Schema fragment.
///
/// Implemented automatically by `#[derive(Serializer)]` for serializer
/// structs (when the `openapi` feature is on). Implement manually for
/// any custom field type used in a serializer.
pub trait OpenApiSchema {
    /// Return the schema describing JSON of this type.
    fn openapi_schema() -> Schema;
}

// Helper: create a [`Schema`] from a Serializer type without naming the trait.
impl Schema {
    /// Shortcut: `Schema::for_serializer::<MySerializer>()`.
    #[must_use]
    pub fn for_serializer<S: OpenApiSchema>() -> Schema {
        S::openapi_schema()
    }
}

// =====================================================================
// Primitives
// =====================================================================

macro_rules! impl_int {
    ($($t:ty => $fmt:literal),* $(,)?) => {
        $(
            impl OpenApiSchema for $t {
                fn openapi_schema() -> Schema {
                    let mut s = Schema::integer();
                    s.format = Some($fmt.into());
                    s
                }
            }
        )*
    };
}

impl_int!(
    i8 => "int32",
    i16 => "int32",
    i32 => "int32",
    i64 => "int64",
    isize => "int64",
    u8 => "int32",
    u16 => "int32",
    u32 => "int64",
    u64 => "int64",
    usize => "int64",
);

impl OpenApiSchema for f32 {
    fn openapi_schema() -> Schema {
        let mut s = Schema::number();
        s.format = Some("float".into());
        s
    }
}
impl OpenApiSchema for f64 {
    fn openapi_schema() -> Schema {
        let mut s = Schema::number();
        s.format = Some("double".into());
        s
    }
}

impl OpenApiSchema for bool {
    fn openapi_schema() -> Schema {
        Schema::boolean()
    }
}

impl OpenApiSchema for String {
    fn openapi_schema() -> Schema {
        Schema::string()
    }
}
impl OpenApiSchema for &str {
    fn openapi_schema() -> Schema {
        Schema::string()
    }
}
impl OpenApiSchema for char {
    fn openapi_schema() -> Schema {
        Schema::string().min_length(1).max_length(1)
    }
}

// =====================================================================
// Container types (transparent forwarding for Option/Vec/Box/etc)
// =====================================================================

impl<T: OpenApiSchema> OpenApiSchema for Option<T> {
    /// `Option<T>` returns the inner schema. The macro adds `.nullable()`
    /// itself so the produced schema reflects that the field can be null.
    /// Doing it here too would double-mark.
    fn openapi_schema() -> Schema {
        T::openapi_schema()
    }
}

impl<T: OpenApiSchema> OpenApiSchema for Vec<T> {
    fn openapi_schema() -> Schema {
        Schema::array_of(T::openapi_schema())
    }
}

impl<T: OpenApiSchema, const N: usize> OpenApiSchema for [T; N] {
    fn openapi_schema() -> Schema {
        Schema::array_of(T::openapi_schema())
    }
}

impl<T: OpenApiSchema> OpenApiSchema for &[T] {
    fn openapi_schema() -> Schema {
        Schema::array_of(T::openapi_schema())
    }
}

impl<T: OpenApiSchema> OpenApiSchema for Box<T> {
    fn openapi_schema() -> Schema {
        T::openapi_schema()
    }
}

impl<T: OpenApiSchema> OpenApiSchema for std::sync::Arc<T> {
    fn openapi_schema() -> Schema {
        T::openapi_schema()
    }
}

/// `Auto<T>` represents a server-assigned value (`SERIAL` /
/// `BIGSERIAL` / `gen_random_uuid()` / DB DEFAULT NOW()) — clients
/// don't supply it on create, but it's always present on read. The
/// schema mirrors `T`. Surfaced when `#[derive(Serializer)]` runs
/// against any model that uses `Auto<T>`.
impl<T: OpenApiSchema> OpenApiSchema for crate::sql::Auto<T> {
    fn openapi_schema() -> Schema {
        T::openapi_schema()
    }
}

impl<V: OpenApiSchema> OpenApiSchema for HashMap<String, V> {
    fn openapi_schema() -> Schema {
        let mut s = Schema::object();
        s.additional_properties = Some(Box::new(V::openapi_schema()));
        s
    }
}

impl<V: OpenApiSchema> OpenApiSchema for BTreeMap<String, V> {
    fn openapi_schema() -> Schema {
        let mut s = Schema::object();
        s.additional_properties = Some(Box::new(V::openapi_schema()));
        s
    }
}

impl<V: OpenApiSchema> OpenApiSchema for IndexMap<String, V> {
    fn openapi_schema() -> Schema {
        let mut s = Schema::object();
        s.additional_properties = Some(Box::new(V::openapi_schema()));
        s
    }
}

// =====================================================================
// chrono / uuid / serde_json — always-on workspace deps
// =====================================================================

impl OpenApiSchema for chrono::DateTime<chrono::Utc> {
    fn openapi_schema() -> Schema {
        Schema::datetime()
    }
}
impl OpenApiSchema for chrono::DateTime<chrono::FixedOffset> {
    fn openapi_schema() -> Schema {
        Schema::datetime()
    }
}
impl OpenApiSchema for chrono::NaiveDateTime {
    fn openapi_schema() -> Schema {
        Schema::datetime()
    }
}
impl OpenApiSchema for chrono::NaiveDate {
    fn openapi_schema() -> Schema {
        Schema::date()
    }
}
impl OpenApiSchema for chrono::NaiveTime {
    fn openapi_schema() -> Schema {
        let mut s = Schema::string();
        s.format = Some("time".into());
        s
    }
}

impl OpenApiSchema for uuid::Uuid {
    fn openapi_schema() -> Schema {
        Schema::uuid()
    }
}

impl OpenApiSchema for serde_json::Value {
    /// Free-form JSON — any shape is allowed.
    fn openapi_schema() -> Schema {
        Schema::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn type_of<T: OpenApiSchema>() -> Value {
        serde_json::to_value(T::openapi_schema()).unwrap()
    }

    #[test]
    fn primitives_map_to_correct_types_and_formats() {
        assert_eq!(type_of::<i32>()["type"], "integer");
        assert_eq!(type_of::<i32>()["format"], "int32");
        assert_eq!(type_of::<i64>()["format"], "int64");
        assert_eq!(type_of::<u32>()["format"], "int64");
        assert_eq!(type_of::<f32>()["format"], "float");
        assert_eq!(type_of::<f64>()["format"], "double");
        assert_eq!(type_of::<bool>()["type"], "boolean");
        assert_eq!(type_of::<String>()["type"], "string");
        assert_eq!(type_of::<char>()["minLength"], 1);
    }

    #[test]
    fn vec_becomes_array() {
        let v = type_of::<Vec<String>>();
        assert_eq!(v["type"], "array");
        assert_eq!(v["items"]["type"], "string");
    }

    #[test]
    fn nested_vec_of_vec() {
        let v = type_of::<Vec<Vec<i32>>>();
        assert_eq!(v["type"], "array");
        assert_eq!(v["items"]["type"], "array");
        assert_eq!(v["items"]["items"]["type"], "integer");
    }

    #[test]
    fn option_passes_through_inner() {
        // The `.nullable()` modifier is added by the macro at the
        // property site, not here, so the inner schema is unchanged.
        let v = type_of::<Option<String>>();
        assert_eq!(v["type"], "string");
        assert!(v.get("nullable").is_none());
    }

    #[test]
    fn hashmap_string_to_t_uses_additional_properties() {
        let v = type_of::<HashMap<String, i64>>();
        assert_eq!(v["type"], "object");
        assert_eq!(v["additionalProperties"]["type"], "integer");
    }

    #[test]
    fn chrono_datetime_uses_date_time_format() {
        let v = type_of::<chrono::DateTime<chrono::Utc>>();
        assert_eq!(v["type"], "string");
        assert_eq!(v["format"], "date-time");
    }

    #[test]
    fn chrono_naivedate_uses_date_format() {
        let v = type_of::<chrono::NaiveDate>();
        assert_eq!(v["format"], "date");
    }

    #[test]
    fn uuid_uses_uuid_format() {
        let v = type_of::<uuid::Uuid>();
        assert_eq!(v["format"], "uuid");
    }

    #[test]
    fn json_value_is_unconstrained() {
        let v = type_of::<serde_json::Value>();
        assert!(v.as_object().map_or(true, |o| o.is_empty()));
    }

    #[test]
    fn box_and_arc_pass_through() {
        assert_eq!(type_of::<Box<i32>>()["type"], "integer");
        assert_eq!(type_of::<std::sync::Arc<bool>>()["type"], "boolean");
    }

    #[test]
    fn schema_for_serializer_works_with_any_impl() {
        struct Custom;
        impl OpenApiSchema for Custom {
            fn openapi_schema() -> Schema {
                Schema::string().description("custom")
            }
        }
        let s = Schema::for_serializer::<Custom>();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "string");
        assert_eq!(v["description"], "custom");
    }
}
