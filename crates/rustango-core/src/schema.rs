//! Schema types: what every model in the registry looks like at runtime.

use crate::FieldType;

/// Static description of a single column on a model.
///
/// `max_length`, `min`, `max` carry per-field bounds populated from
/// `#[rustango(max_length = …, min = …, max = …)]`. The query layer
/// uses them to validate writes; a future migration writer will use
/// them to emit `VARCHAR(N)` and `CHECK` constraints.
#[derive(Debug, Clone, Copy)]
pub struct FieldSchema {
    pub name: &'static str,
    pub column: &'static str,
    pub ty: FieldType,
    pub nullable: bool,
    pub primary_key: bool,
    pub relation: Option<Relation>,
    /// Maximum string length in characters. Only meaningful for `FieldType::String`.
    pub max_length: Option<u32>,
    /// Inclusive integer lower bound. Only meaningful for `I32`/`I64`.
    pub min: Option<i64>,
    /// Inclusive integer upper bound. Only meaningful for `I32`/`I64`.
    pub max: Option<i64>,
}

/// Static description of a relation to another model.
///
/// v0.1 only emits `FK` and `O2O`. `M2M` is reserved.
#[derive(Debug, Clone, Copy)]
pub enum Relation {
    /// Foreign key. The local column references `to.<on>`.
    Fk { to: &'static str, on: &'static str },
    /// One-to-one. Same shape as FK, separate variant for callers that care.
    O2O { to: &'static str, on: &'static str },
    /// Many-to-many through a join table. Reserved for v0.2.
    M2M {
        to: &'static str,
        through: &'static str,
        src: &'static str,
        dst: &'static str,
    },
}

/// Static description of a model.
///
/// `display` is the Rust-side field name that should be used when
/// rendering this model as the *target* of a foreign key — admin UIs
/// and any future "select" widgets render `display`'s value rather than
/// the raw PK. Set via `#[rustango(display = "field")]`; defaults to
/// `None`, in which case callers fall back to the primary key.
#[derive(Debug, Clone, Copy)]
pub struct ModelSchema {
    pub name: &'static str,
    pub table: &'static str,
    pub fields: &'static [FieldSchema],
    pub display: Option<&'static str>,
}

impl ModelSchema {
    /// Look up a field by its Rust-side name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Look up a field by its SQL column name.
    #[must_use]
    pub fn field_by_column(&self, column: &str) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|f| f.column == column)
    }

    /// The primary-key field, if any. Returns the first `primary_key = true` field.
    #[must_use]
    pub fn primary_key(&self) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|f| f.primary_key)
    }

    /// Iterator over scalar (non-M2M) fields. v0.1 keeps everything scalar.
    pub fn scalar_fields(&self) -> impl Iterator<Item = &'static FieldSchema> {
        self.fields
            .iter()
            .filter(|f| !matches!(f.relation, Some(Relation::M2M { .. })))
    }

    /// Field used to render this model as a foreign-key target.
    ///
    /// Returns the field declared via `#[rustango(display = "…")]`, or
    /// the primary key if no display is set. Returns `None` only for the
    /// (unusual) model with neither a `display` attribute nor a primary key.
    #[must_use]
    pub fn display_field(&self) -> Option<&'static FieldSchema> {
        if let Some(name) = self.display {
            return self.field(name);
        }
        self.primary_key()
    }

    /// Fields that should participate in free-text search (`?q=…` in the
    /// admin). Heuristic: a `String` field with a `max_length` cap is
    /// likely a name/title/short label; long, uncapped strings (bodies,
    /// descriptions) are excluded so search stays cheap.
    pub fn searchable_fields(&self) -> impl Iterator<Item = &'static FieldSchema> {
        self.fields.iter().filter(|f| {
            matches!(f.ty, FieldType::String) && f.max_length.is_some() && f.relation.is_none()
        })
    }
}

/// Trait every `#[derive(Model)]` struct implements.
///
/// Carries the static `SCHEMA` so the registry and the query layer can
/// reach the model's metadata without an instance.
pub trait Model: Sized + Send + Sync + 'static {
    const SCHEMA: &'static ModelSchema;
}

/// Inventory entry submitted by the `#[derive(Model)]` macro for each model.
///
/// Internal API: end users should not construct these directly.
#[doc(hidden)]
pub struct ModelEntry {
    pub schema: &'static ModelSchema,
}

inventory::collect!(ModelEntry);
