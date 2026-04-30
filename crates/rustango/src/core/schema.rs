//! Schema types: what every model in the registry looks like at runtime.

use super::FieldType;

/// Static description of a single column on a model.
///
/// `max_length`, `min`, `max` carry per-field bounds populated from
/// `#[rustango(max_length = …, min = …, max = …)]`. The query layer
/// uses them to validate writes; the migration writer uses them to
/// emit `VARCHAR(N)` and `CHECK` constraints.
///
/// `default` is the raw SQL fragment placed after `DEFAULT` in DDL
/// (e.g. `"0"`, `"'draft'"`, `"NOW()"`). Set via
/// `#[rustango(default = "…")]`. The string is inserted verbatim — it
/// is the developer's responsibility to write a valid Postgres
/// expression and to quote string literals themselves.
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
    /// Raw SQL expression for the column's `DEFAULT` clause, if any.
    pub default: Option<&'static str>,
    /// `true` for fields whose Rust type is `Auto<T>` — server-assigned
    /// PKs that translate to `BIGSERIAL` / `SERIAL` and skip the column
    /// from explicit INSERTs when `Auto::Unset` so Postgres' DEFAULT
    /// fires. The migration writer reads this; the `Auto::Unset → SQL
    /// DEFAULT` translation happens in the macro-generated INSERT path.
    pub auto: bool,
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
    /// Explicit Django-style app label, set via
    /// `#[rustango(app = "blog")]` on the struct. `None` when the user
    /// didn't override it; in that case [`ModelEntry::resolved_app_label`]
    /// falls back to inferring from the registered module path.
    pub app_label: Option<&'static str>,
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
    /// Result of `module_path!()` at the registration site (e.g.
    /// `"my_app::blog::models"`). Used by
    /// [`Self::resolved_app_label`] to infer a Django-style
    /// `app_label` when the user didn't set one explicitly.
    pub module_path: &'static str,
}

impl ModelEntry {
    /// Django-shape app label for this model. Returns the explicit
    /// override from `#[rustango(app = "...")]` if set; otherwise
    /// infers from `module_path` by taking the first segment after the
    /// crate root. Examples (assuming crate `my_app`):
    ///
    /// * `"my_app::blog::models"`  → `Some("blog")`
    /// * `"my_app::shop::models"`  → `Some("shop")`
    /// * `"my_app::models"`        → `None` (top-level project model)
    /// * `"my_app"`                → `None`
    ///
    /// `None` means the model lives at the project root, not inside a
    /// dedicated app. Used for per-app migration discovery, admin
    /// sidebar grouping, and `manage makemigrations <app>` filtering.
    #[must_use]
    pub fn resolved_app_label(&self) -> Option<&'static str> {
        if let Some(label) = self.schema.app_label {
            return Some(label);
        }
        infer_app_label_from_module_path(self.module_path)
    }
}

/// Parse the Rust module path produced by `module_path!()` and return
/// the first segment after the crate root, or `None` when the model
/// lives at the project root. Public so callers (admin, makemigrations
/// CLI, the diagnostic `manage list-apps` verb) can apply the same
/// inference rules to module-path strings they already have.
#[must_use]
pub fn infer_app_label_from_module_path(path: &'static str) -> Option<&'static str> {
    let mut parts = path.split("::");
    let _crate_name = parts.next()?;
    let candidate = parts.next()?;
    // Skip pseudo-segments that mean "still at the project root":
    // `models`, `views`, `urls` are sibling files at `src/`, not apps.
    if matches!(candidate, "models" | "views" | "urls" | "main") {
        return None;
    }
    Some(candidate)
}

inventory::collect!(ModelEntry);

#[cfg(test)]
mod tests {
    use super::infer_app_label_from_module_path as infer;

    #[test]
    fn infers_app_from_submodule() {
        assert_eq!(infer("my_app::blog::models"), Some("blog"));
        assert_eq!(infer("my_app::shop::models"), Some("shop"));
        assert_eq!(infer("my_app::auth"), Some("auth"));
    }

    #[test]
    fn returns_none_for_project_root_models() {
        assert_eq!(infer("my_app"), None);
        assert_eq!(infer("my_app::models"), None);
        assert_eq!(infer("my_app::views"), None);
    }
}
