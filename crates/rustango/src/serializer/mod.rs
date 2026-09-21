//! Serializers — typed JSON built from model instances.
//!
//! A serializer is a struct that maps a [`Model`] to a JSON shape.
//! Field attributes decide what is included, renamed or left out.
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
//! | `nested(strict)` | the same, but panics when the FK is not loaded | included | no |
//! | `many = TagSerializer` | starts empty; fill it with the `set_<field>(&[Tag])` helper | included | no |
//! | `slug = "name"` | clones `model.<source>.value()?.name` | included | no |
//! | `validate = "fn"` | per-field validator, run by `Self::validate(&self)` | n/a | n/a |
//! | `max_length = N` | caps string length on write | n/a | n/a |
//! | `min_length = N` | requires a minimum string length on write | n/a | n/a |
//! | `min = N` / `max = N` | inclusive integer bounds on write | n/a | n/a |
//!
//! ## Field validators
//!
//! `max_length`, `min_length`, `min` and `max` are checked on write,
//! that is on create and update through a ViewSet, and a failure
//! becomes a `400`.
//!
//! You often need no attribute at all: every writable field is
//! checked against the model's [`crate::core::FieldSchema`], which
//! carries `max_length`, `min`, `max` and `choices`. An attribute
//! overrides what the model says. `min_length` has no model column,
//! so it only exists here. String length counts characters.
//!
//! For any other rule use `validate = "fn"` on a field, or the
//! container `validate` across fields.
//!
//! ## Nested serializers: `#[serializer(nested)]`
//!
//! When the field is another serializer and `select_related` already
//! loaded the FK, the macro walks the FK for you:
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
//! If the FK was not loaded, the field falls back to
//! `Default::default()` instead of panicking, so production degrades
//! gently. Use `#[serializer(nested(strict))]` to panic instead,
//! which is useful in tests.
//!
//! For a list of children, use
//! `#[serializer(many = ChildSerializer)]`. The macro emits a
//! `set_<field>(&[Child])` setter. Fetch the children yourself and
//! call it after `from_model`; it cannot happen automatically,
//! because the M2M accessor is async.
//!
//! ## Computed fields: `#[serializer(method = "fn")]`
//!
//! The macro calls `Self::fn(&model)` to fill the field:
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
//! For rules that span fields, write `validate(&self)` on the
//! serializer:
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
//! For one field, put `#[serializer(validate = "fn_name")]` on it and
//! write `fn fn_name(value: &T) -> Result<(), String>` as an
//! associated method. The generated `validate(&self)` collects every
//! field's result into one `FormErrors`.

use serde_json::Value;

/// The serializer trait. `#[derive(Serializer)]` implements it.
///
/// The macro writes `from_model` and `writable_fields` for you. The
/// rest have defaults: `to_value` goes through `serde::Serialize`,
/// `many` and `many_to_value` loop over `from_model`, and `validate`
/// does nothing until you override it.
pub trait ModelSerializer: serde::Serialize + Sized {
    /// The [`crate::core::Model`] type this serializer maps from.
    type Model;

    /// Build a serializer from a model.
    ///
    /// Plain and `read_only` fields are cloned from the model.
    /// `write_only` and `skip` fields get their default; set those
    /// yourself afterwards if you need them.
    fn from_model(model: &Self::Model) -> Self;

    /// Turn this instance into JSON. `write_only` fields are left
    /// out.
    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Build one serializer per model.
    fn many(models: &[Self::Model]) -> Vec<Self> {
        models.iter().map(Self::from_model).collect()
    }

    /// Turn a slice of models straight into a JSON array.
    fn many_to_value(models: &[Self::Model]) -> Value {
        Value::Array(
            models
                .iter()
                .map(|m| Self::from_model(m).to_value())
                .collect(),
        )
    }

    /// Field names a create or update request may set. `read_only`
    /// and `skip` fields are not in the list. The ViewSet write path
    /// uses it to filter the incoming JSON.
    fn writable_fields() -> &'static [&'static str];

    /// The same fields, but under their **model** names. A field with
    /// `#[serializer(source = "x")]` appears as `"x"`; any other
    /// appears under its own name.
    ///
    /// The ViewSet write path ignores every model column outside this
    /// set, so a client cannot write a `read_only` or computed field.
    /// The default is [`Self::writable_fields`], which is right when
    /// nothing is renamed; the macro overrides it when something is.
    fn writable_source_fields() -> &'static [&'static str] {
        Self::writable_fields()
    }

    /// Parse a JSON request body for validation. Writable fields are
    /// read by their serializer name; read-only and computed fields
    /// get their default. A type error lands in
    /// [`crate::forms::FormErrors`] under the field's name.
    ///
    /// The macro writes this. The default returns an error, so a
    /// hand-written impl that forgets it fails loudly instead of
    /// skipping validation.
    ///
    /// # Errors
    /// A `FormErrors` naming every field that failed to parse.
    fn from_writable_json(body: &Value) -> Result<Self, crate::forms::FormErrors> {
        let _ = body;
        let mut errors = crate::forms::FormErrors::default();
        errors.add_non_field("from_writable_json not implemented for this serializer");
        Err(errors)
    }

    /// Run the serializer's validators. The macro overrides this when
    /// the serializer declares any `validate = "..."`, on a field or
    /// on the container. By default it does nothing.
    ///
    /// # Errors
    /// A [`crate::forms::FormErrors`] holding every failed rule.
    fn validate(&self) -> Result<(), crate::forms::FormErrors> {
        Ok(())
    }
}

/// Check, before a save, that a row does not collide with an
/// existing one on any of the model's `unique_together` constraints.
///
/// Returns `Ok(())` when nothing collides, or a `FormErrors` with one
/// non-field error per clashing constraint.
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
/// On an update, pass `exclude_pk = Some(&pk)` so the row being
/// edited does not collide with itself. Pass `None` on an insert.
///
/// Partial unique indexes are skipped, because whether they conflict
/// depends on a predicate this function does not evaluate.
///
/// # Errors
/// A failed query returns `Err` rather than reporting "no
/// collision".
pub async fn check_unique_together_pool(
    pool: &crate::sql::Pool,
    schema: &'static crate::core::ModelSchema,
    values: &std::collections::HashMap<&'static str, crate::core::SqlValue>,
    exclude_pk: Option<&crate::core::SqlValue>,
) -> Result<(), crate::forms::FormErrors> {
    use crate::core::{Filter, Op};

    let mut errors = crate::forms::FormErrors::default();

    for index in schema.indexes {
        // Only multi-column unique indexes. A single-column UNIQUE is
        // the field's own `unique` attribute, which the INSERT
        // catches. Partial unique indexes need predicate evaluation,
        // which this layer does not do.
        if !index.unique || index.columns.len() < 2 || index.where_clause.is_some() {
            continue;
        }

        // Build a WHERE of ANDed equalities. If a column has no
        // value, skip the whole constraint: a partly bound set cannot
        // be checked for a collision.
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

        // On an update, exclude the row being edited.
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

        // The value does not matter; any row at all is a collision.
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

// Helpers for hyperlinked output. A normal serializer emits keys such
// as `{"id": 42, "author_id": 7}`; a hyperlinked one emits
// `{"url": "/api/posts/42", "author_url": "/api/users/7"}`. You
// could do this with `#[serializer(method = "url")]` and a hand-
// written function, but these two cover the common case by filling
// in `{pk}` in a URL template.

/// Replace every `{pk}` in `template` with the primary key.
///
/// Numbers print as numbers, and strings and UUIDs print as
/// themselves. Anything else falls back to its debug form, which is
/// rare: almost every key is an integer, a UUID or a string.
///
/// ```ignore
/// use rustango::core::SqlValue;
/// let url = rustango::serializer::hyperlink_url("/api/posts/{pk}", &SqlValue::I64(42));
/// assert_eq!(url, "/api/posts/42");
/// ```
#[must_use]
pub fn hyperlink_url(template: &str, pk: &crate::core::SqlValue) -> String {
    let pk_str = render_pk(pk);
    template.replace("{pk}", &pk_str)
}

/// Add a `url` field, built from the primary key, and one
/// `<fk>_url` field per named FK template, to a serializer's JSON.
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
/// What it does:
///
/// - Adds `url`, by putting `base[pk_field]` into `self_template`.
/// - For each `(fk_field, template)`, adds a `<fk_field>_url` key.
///   A missing or null FK gives a null URL.
/// - Keeps the original `id` and `<fk>_id` keys. Remove them
///   afterwards if you do not want them.
///
/// # Panics
/// When `base` is not a JSON object. A serializer's `to_value`
/// always returns one.
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

    // The object's own URL, from base[pk_field].
    if let Some(pk_val) = obj.get(pk_field) {
        let pk_str = render_pk_json(pk_val);
        let url = self_template.replace("{pk}", &pk_str);
        obj.insert("url".into(), serde_json::Value::String(url));
    }

    // One `<field>_url` per template. A missing or null FK gives a
    // null URL.
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
        // The other variants are not realistic primary keys. Use the
        // debug form, so the URL is at least not empty.
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
        // The original fields stay.
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
        // No `id` in the input means no `url` in the output, and no
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
