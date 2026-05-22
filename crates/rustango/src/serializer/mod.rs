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

/// Django-shape `UniqueTogetherValidator` — pre-save check that a
/// candidate row doesn't collide with an existing row on any of the
/// model's declared `unique_together` constraints. Issue #437.
///
/// Returns `Ok(())` when no collision is detected. Returns `Err(FormErrors)`
/// with a non-field error per colliding constraint (DRF shape:
/// `"The fields a, b must be unique together"`).
///
/// ## Usage
///
/// ```ignore
/// use std::collections::HashMap;
/// use rustango::core::SqlValue;
/// use rustango::serializer::check_unique_together_pool;
///
/// let mut values: HashMap<&'static str, SqlValue> = HashMap::new();
/// values.insert("org_id", SqlValue::I64(self.org_id));
/// values.insert("user_id", SqlValue::I64(self.user_id));
/// check_unique_together_pool(&pool, Membership::SCHEMA, &values, None).await?;
/// ```
///
/// Pass `exclude_pk = Some(&pk)` on updates so the row being edited
/// doesn't collide with itself. The PK column is read off
/// `ModelSchema::primary_key()` — pass `None` for inserts.
///
/// Partial unique constraints (`unique_when` / `WHERE`-clause partial
/// indexes) are skipped — their conflict semantics depend on the
/// predicate, which this layer doesn't evaluate.
///
/// # Errors
/// - [`crate::sql::ExecError`] forwarded from the underlying query.
/// - The check is non-fatal on errors: a query failure surfaces as
///   `Err` rather than masking as "no collision".
pub async fn check_unique_together_pool(
    pool: &crate::sql::Pool,
    schema: &'static crate::core::ModelSchema,
    values: &std::collections::HashMap<&'static str, crate::core::SqlValue>,
    exclude_pk: Option<&crate::core::SqlValue>,
) -> Result<(), crate::forms::FormErrors> {
    use crate::core::{Filter, Op};

    let mut errors = crate::forms::FormErrors::default();

    for index in schema.indexes {
        // Only multi-column unique indexes — Django's `unique_together`.
        // Single-column UNIQUE is the `unique` field attr (caught by
        // INSERT's RETURNING-on-conflict). Partial unique
        // (`where_clause = Some(_)`) skipped — predicate evaluation
        // would need a stub planner we don't have today.
        if !index.unique || index.columns.len() < 2 || index.where_clause.is_some() {
            continue;
        }

        // Build the AND-of-equality WHERE clause. Missing column values
        // skip the constraint — Django's behavior when only a partial
        // subset of the unique-together fields is bound.
        let mut predicates: Vec<Filter> = Vec::with_capacity(index.columns.len());
        let mut all_bound = true;
        for col in index.columns {
            let Some(val) = values.get(*col) else {
                all_bound = false;
                break;
            };
            predicates.push(Filter {
                column: col,
                op: Op::Eq,
                value: val.clone(),
            });
        }
        if !all_bound {
            continue;
        }

        // Exclude-self on updates: PK != $N.
        if let (Some(pk_field), Some(pk_value)) = (schema.primary_key(), exclude_pk) {
            predicates.push(Filter {
                column: pk_field.column,
                op: Op::Ne,
                value: pk_value.clone(),
            });
        }

        // SELECT 1 FROM table WHERE … LIMIT 1.
        let dialect = pool.dialect();
        let table_q = dialect.quote_ident(schema.table);
        let mut clauses: Vec<String> = Vec::with_capacity(predicates.len());
        let mut params: Vec<crate::core::SqlValue> = Vec::with_capacity(predicates.len());
        for (i, pred) in predicates.iter().enumerate() {
            let col = dialect.quote_ident(pred.column);
            let op_str = match pred.op {
                Op::Eq => "=",
                Op::Ne => "<>",
                _ => unreachable!("only Eq/Ne above"),
            };
            let placeholder = dialect.placeholder(i + 1);
            clauses.push(format!("{col} {op_str} {placeholder}"));
            params.push(pred.value.clone());
        }
        let where_sql = clauses.join(" AND ");
        let sql = format!("SELECT 1 FROM {table_q} WHERE {where_sql} LIMIT 1");

        // Use `raw_query_pool::<(i64,)>` and ignore the actual returned
        // value — presence of any row means a collision.
        let hits: Vec<(i64,)> = crate::sql::raw_query_pool(&sql, params, pool)
            .await
            .map_err(|e| {
                let mut errs = crate::forms::FormErrors::default();
                errs.add_non_field(format!("unique_together check failed: {e}"));
                errs
            })?;
        if !hits.is_empty() {
            errors.add_non_field(format!(
                "The fields {} must be unique together.",
                index.columns.join(", "),
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
