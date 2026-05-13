//! DRF-style serializer layer — typed JSON output from model instances.
//!
//! A serializer is a Rust struct that maps a [`Model`] instance to a
//! JSON-ready shape, with per-field control over what is included,
//! renamed, or excluded.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::Serializer;
//! use rustango::serializer::ModelSerializer;
//!
//! #[derive(Serializer, serde::Deserialize, Default)]
//! #[serializer(model = Post)]
//! pub struct PostSerializer {
//!     pub id:         i64,
//!     pub title:      String,
//!     #[serializer(read_only)]
//!     pub created_at: chrono::DateTime<chrono::Utc>,
//!     #[serializer(write_only)]
//!     pub secret:     String,
//!     #[serializer(source = "body")]
//!     pub content:    String,
//!     #[serializer(skip)]
//!     pub tag_ids:    Vec<i64>,   // set manually: s.tag_ids = post.tags_m2m().all(&pool).await?
//! }
//!
//! // Serialize:
//! let s = PostSerializer::from_model(&post);
//! let json = s.to_value();
//!
//! // Serialize many:
//! let json_array = PostSerializer::many_to_value(&posts);
//! ```
//!
//! ## Field attributes
//!
//! | Attribute | Effect on `from_model` | Effect on JSON output | Effect on `writable_fields` |
//! |---|---|---|---|
//! | *(none)* | mapped from model | included | yes |
//! | `read_only` | mapped from model | included | no |
//! | `write_only` | `Default::default()` | excluded | yes |
//! | `source = "x"` | mapped from `model.x` | included | yes |
//! | `skip` | `Default::default()` | included | no |
//! | `method = "fn"` | calls `Self::fn(&model)` | included | no |
//! | `nested` | reads `model.<field>.value()` then `Child::from_model(parent)` | included | no |
//! | `nested(strict)` | same, but panics on unloaded FK | included | no |
//! | `many = TagSerializer` | initializes to `Vec::new()`; populate via `set_<field>(&[Tag])` helper | included | no |
//! | `slug = "name"` | clones `model.<source>.value()?.name` (DRF SlugRelatedField) | included | no |
//! | `validate = "fn"` | per-field validator called by `Self::validate(&self)` | n/a | n/a |
//!
//! ## Nested serializers — auto-resolved via `#[serializer(nested)]`
//!
//! When the field type is another serializer and the model's FK is
//! already loaded (via `select_related`), the macro emits a `from_model`
//! initializer that walks the FK automatically:
//!
//! ```ignore
//! #[derive(Serializer, serde::Deserialize, Default)]
//! #[serializer(model = Post)]
//! struct PostWithAuthor {
//!     pub id: i64,
//!     pub title: String,
//!     #[serializer(nested)]
//!     pub author: AuthorSerializer,
//! }
//! ```
//!
//! If the FK was *not* loaded (no `select_related`), the field falls
//! back to `Default::default()` rather than panicking — production
//! degrades gracefully. Use `#[serializer(nested(strict))]` to opt
//! back into the v0.18.1 panic-on-unloaded behaviour for tests.
//!
//! For lists of children (one-to-many / M2M), use
//! `#[serializer(many = ChildSerializer)]`. The macro emits a
//! `set_<field>(&[Child])` setter; the caller fetches the children
//! and calls it after `from_model` (auto-load isn't possible because
//! the M2M accessor is async).
//!
//! ## Computed fields — `#[serializer(method = "fn")]`
//!
//! DRF `SerializerMethodField` analog. The macro emits a `from_model`
//! initializer that calls `Self::fn(&model)`:
//!
//! ```ignore
//! impl PostSerializer {
//!     fn excerpt(model: &Post) -> String {
//!         model.body.chars().take(80).collect::<String>() + "…"
//!     }
//! }
//!
//! #[derive(Serializer, serde::Deserialize, Default)]
//! #[serializer(model = Post)]
//! struct PostSerializer {
//!     pub title: String,
//!     #[serializer(method = "excerpt")]
//!     pub excerpt: String,
//! }
//! ```
//!
//! ## Validation
//!
//! Cross-field validation: implement `validate(&self)` as an inherent
//! method on the serializer struct:
//!
//! ```ignore
//! impl PostSerializer {
//!     pub fn validate(&self) -> Result<(), rustango::forms::FormErrors> {
//!         let mut errors = rustango::forms::FormErrors::default();
//!         if self.title.is_empty() {
//!             errors.add("title", "title cannot be empty");
//!         }
//!         if errors.is_empty() { Ok(()) } else { Err(errors) }
//!     }
//! }
//! ```
//!
//! Per-field validators: declare `#[serializer(validate = "fn_name")]`
//! on the field and write `fn fn_name(value: &T) -> Result<(), String>`
//! as an associated method. The macro-generated `validate(&self)`
//! aggregates per-field results into a `FormErrors`.

use serde_json::Value;

/// Core serializer trait. Implemented by `#[derive(Serializer)]` structs.
///
/// # Required implementations
///
/// The derive macro generates:
/// - `from_model` — maps a model instance to the serializer struct
/// - `writable_fields` — field names accepted on create/update (excludes `read_only` and `skip`)
///
/// # Default implementations
///
/// - `to_value` — calls `serde::Serialize` (which the macro also emits, skipping `write_only` fields)
/// - `many` / `many_to_value` — batch `from_model` calls
/// - `validate` — no-op; override to add cross-field validation
pub trait ModelSerializer: serde::Serialize + Sized {
    /// The [`crate::core::Model`] type this serializer maps from.
    type Model;

    /// Construct a serializer from a model instance.
    ///
    /// `read_only` and normal fields are cloned from the model.
    /// `write_only` and `skip` fields are `Default::default()` —
    /// set them manually after calling this if needed.
    fn from_model(model: &Self::Model) -> Self;

    /// Serialize this instance to a JSON value.
    ///
    /// Uses the `serde::Serialize` implementation emitted by the derive
    /// macro, which respects `write_only` (those fields are excluded).
    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Serialize a slice of model instances into a `Vec` of serializers.
    fn many(models: &[Self::Model]) -> Vec<Self> {
        models.iter().map(Self::from_model).collect()
    }

    /// Serialize a slice of model instances directly to a JSON array.
    fn many_to_value(models: &[Self::Model]) -> Value {
        Value::Array(
            models
                .iter()
                .map(|m| Self::from_model(m).to_value())
                .collect(),
        )
    }

    /// Field names accepted on create/update requests (excludes `read_only`
    /// and `skip` fields). Used by the ViewSet write path to filter the
    /// incoming JSON body.
    fn writable_fields() -> &'static [&'static str];
}
