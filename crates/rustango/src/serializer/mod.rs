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
//! | `max_length = N` | caps string length on write (DRF `MaxLengthValidator`) | n/a | n/a |
//! | `min_length = N` | min string length on write (DRF `MinLengthValidator`) | n/a | n/a |
//! | `min = N` / `max = N` | inclusive integer bounds on write (DRF `Min/MaxValueValidator`) | n/a | n/a |
//!
//! ## Declarative field validators
//!
//! `max_length` / `min_length` / `min` / `max` are checked on **write**
//! (create/update through a ViewSet) and surface DRF-shape `400`s. They
//! **auto-inherit from the model**: every writable field is validated
//! against the model's [`crate::core::FieldSchema`] (`max_length`, `min`,
//! `max`, and `choices`) even with no attribute; a per-field attribute
//! overrides the inherited value. `min_length` is serializer-only (no
//! model column). `choices` is inherited from the model (no attribute).
//! String length is measured in characters. For arbitrary rules, use
//! `validate = "fn"` (per-field) or the container `validate` (cross-field).
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

    /// The **model** field names of the writable serializer fields
    /// (`source`-resolved). The ViewSet write path skips every model
    /// column NOT in this set, so `read_only` / `method` / computed
    /// fields a client posts are ignored instead of written.
    ///
    /// For a field with `#[serializer(source = "x")]` this is `"x"`; for
    /// a plain field it's the field name. Defaults to
    /// [`Self::writable_fields`] (correct when no `source` rename is in
    /// play); the derive macro overrides it with the resolved names.
    fn writable_source_fields() -> &'static [&'static str] {
        Self::writable_fields()
    }

    /// Build a partial instance from a JSON request body for **input
    /// validation**: writable fields are parsed (by serializer field
    /// name); read-only / computed fields default. Per-field type errors
    /// land in [`crate::forms::FormErrors`] keyed by the field name. The
    /// derive macro generates this; the default errors out so a manual
    /// impl that forgets it fails loudly rather than silently skipping
    /// validation.
    ///
    /// # Errors
    /// A `FormErrors` carrying every field that failed to parse.
    fn from_writable_json(body: &Value) -> Result<Self, crate::forms::FormErrors> {
        let _ = body;
        let mut errors = crate::forms::FormErrors::default();
        errors.add_non_field("from_writable_json not implemented for this serializer");
        Err(errors)
    }

    /// Cross-field / per-field validation hook (DRF `validate`). The
    /// derive macro overrides this when the serializer declares any
    /// `#[serializer(validate = "...")]` field validator or a container
    /// `validate = "..."` cross-field method. Default: no-op.
    ///
    /// # Errors
    /// A [`crate::forms::FormErrors`] aggregating every failed rule.
    fn validate(&self) -> Result<(), crate::forms::FormErrors> {
        Ok(())
    }
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

// ============================================================ #434
//
// Django/DRF-shape `HyperlinkedModelSerializer`. Where a regular
// serializer emits PKs (`{"id": 42, "author_id": 7}`), a
// hyperlinked one emits resource URLs (`{"url": "/api/posts/42",
// "author_url": "/api/users/7"}`). rustango's Serializer derive
// is already rich enough that the user can roll their own with
// `#[serializer(method = "url")]` + a manual `fn url(&self) -> String`,
// but that's boilerplate-heavy for the common case. These free
// functions cover the 95% case — substitute `{pk}` placeholders
// in a URL template.

/// Substitute `{pk}` in `template` with the formatted PK value.
///
/// `pk` formats as: integers/floats render their numeric form,
/// strings/UUIDs render their `Display`, everything else falls
/// back to JSON encoding (rare in practice — most PK fields are
/// integer / UUID / string).
///
/// ```ignore
/// use rustango::core::SqlValue;
/// let url = rustango::serializer::hyperlink_url("/api/posts/{pk}", &SqlValue::I64(42));
/// assert_eq!(url, "/api/posts/42");
/// ```
///
/// Every `{pk}` occurrence is substituted — useful for nested
/// resource URLs.
#[must_use]
pub fn hyperlink_url(template: &str, pk: &crate::core::SqlValue) -> String {
    let pk_str = render_pk(pk);
    template.replace("{pk}", &pk_str)
}

/// Wrap a serializer's JSON output with a `url` field (from the
/// model's PK) and optional `<fk>_url` fields (from named FK
/// templates). Issue #434.
///
/// ```ignore
/// use rustango::serializer::{hyperlinked_to_value, Serializer};
/// use std::collections::HashMap;
///
/// let post = Post::objects().get_by_pk_pool(&pool, &42).await?;
/// let ser = PostSerializer::from_model(&post);
/// let base = ser.to_value();
///
/// let mut fk_templates = HashMap::new();
/// fk_templates.insert("author_id", "/api/users/{pk}");
///
/// let hyperlinked = hyperlinked_to_value(
///     base,
///     "/api/posts/{pk}",
///     "id",                  // pk field name on the serializer output
///     &fk_templates,
/// );
/// // {"url": "/api/posts/42", "author_id_url": "/api/users/7", ...}
/// ```
///
/// Behaviour:
///
/// - Adds a `url` field to the top-level object, derived by
///   substituting `{pk}` in `self_template` with `base[pk_field]`.
/// - For each `(fk_field, template)` in `fk_templates`, looks up
///   `base[fk_field]` and emits a sibling `<fk_field>_url` key
///   with the substituted URL. Null / missing FK values produce
///   a null URL (matches DRF's behavior on nullable FKs).
/// - Does NOT remove the original `id` / `<fk>_id` keys. DRF's
///   `HyperlinkedModelSerializer` also keeps them by default;
///   apps that want to redact them can `obj.as_object_mut()
///   .remove("id")` after this call.
///
/// Panics if `base` isn't a JSON object (the standard serializer
/// `to_value` always returns one).
#[must_use]
pub fn hyperlinked_to_value(
    mut base: serde_json::Value,
    self_template: &str,
    pk_field: &str,
    fk_templates: &std::collections::HashMap<&str, &str>,
) -> serde_json::Value {
    let obj = base
        .as_object_mut()
        .expect("hyperlinked_to_value: base must be a JSON object");

    // Self URL — derived from base[pk_field].
    if let Some(pk_val) = obj.get(pk_field) {
        let pk_str = render_pk_json(pk_val);
        let url = self_template.replace("{pk}", &pk_str);
        obj.insert("url".into(), serde_json::Value::String(url));
    }

    // FK URLs — one per (field, template) pair. Output key is
    // `<field>_url`. Missing / null FK values emit a JSON null URL.
    for (fk_field, template) in fk_templates {
        let url_key = format!("{fk_field}_url");
        match obj.get(*fk_field) {
            Some(v) if !v.is_null() => {
                let pk_str = render_pk_json(v);
                let url = template.replace("{pk}", &pk_str);
                obj.insert(url_key, serde_json::Value::String(url));
            }
            _ => {
                obj.insert(url_key, serde_json::Value::Null);
            }
        }
    }

    base
}

fn render_pk(pk: &crate::core::SqlValue) -> String {
    use crate::core::SqlValue;
    match pk {
        SqlValue::I16(v) => v.to_string(),
        SqlValue::I32(v) => v.to_string(),
        SqlValue::I64(v) => v.to_string(),
        SqlValue::F32(v) => v.to_string(),
        SqlValue::F64(v) => v.to_string(),
        SqlValue::String(s) => s.clone(),
        SqlValue::Uuid(u) => u.to_string(),
        // Other variants (Bool / Json / Date / DateTime / Decimal /
        // Binary / Time / Null / List / Array / RangeLiteral) are
        // not realistic PK shapes — fall back to the Debug
        // repr so the URL is at least non-empty, but don't try
        // hard.
        other => format!("{other:?}"),
    }
}

fn render_pk_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod hyperlinked_tests {
    use super::*;

    #[test]
    fn hyperlink_url_substitutes_i64_pk() {
        let url = hyperlink_url("/api/posts/{pk}", &crate::core::SqlValue::I64(42));
        assert_eq!(url, "/api/posts/42");
    }

    #[test]
    fn hyperlink_url_substitutes_string_pk() {
        let url = hyperlink_url(
            "/users/{pk}",
            &crate::core::SqlValue::String("alice".into()),
        );
        assert_eq!(url, "/users/alice");
    }

    #[test]
    fn hyperlink_url_substitutes_every_occurrence() {
        let url = hyperlink_url("/posts/{pk}/comments/{pk}", &crate::core::SqlValue::I64(7));
        assert_eq!(url, "/posts/7/comments/7");
    }

    #[test]
    fn hyperlinked_to_value_adds_url_field_for_self() {
        let base = serde_json::json!({"id": 42, "title": "Hi"});
        let out = hyperlinked_to_value(
            base,
            "/api/posts/{pk}",
            "id",
            &std::collections::HashMap::new(),
        );
        assert_eq!(out["url"], "/api/posts/42");
        // Original fields stay.
        assert_eq!(out["id"], 42);
        assert_eq!(out["title"], "Hi");
    }

    #[test]
    fn hyperlinked_to_value_adds_fk_url_keys() {
        let base = serde_json::json!({
            "id": 1,
            "title": "Hi",
            "author_id": 7,
            "section_id": serde_json::Value::Null,
        });
        let mut fks: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        fks.insert("author_id", "/users/{pk}");
        fks.insert("section_id", "/sections/{pk}");
        let out = hyperlinked_to_value(base, "/posts/{pk}", "id", &fks);
        assert_eq!(out["author_id_url"], "/users/7");
        // Null FK → null URL.
        assert!(out["section_id_url"].is_null());
    }

    #[test]
    fn hyperlinked_to_value_handles_missing_pk_key_gracefully() {
        // base has no `id` field → no `url` field emitted, but no
        // panic.
        let base = serde_json::json!({"title": "Hi"});
        let out = hyperlinked_to_value(
            base,
            "/api/posts/{pk}",
            "id",
            &std::collections::HashMap::new(),
        );
        assert_eq!(out.get("url"), None);
    }

    #[test]
    fn hyperlinked_to_value_supports_string_pk() {
        let base = serde_json::json!({"slug": "hello", "title": "Hello"});
        let out = hyperlinked_to_value(
            base,
            "/posts/{pk}",
            "slug",
            &std::collections::HashMap::new(),
        );
        assert_eq!(out["url"], "/posts/hello");
    }
}
