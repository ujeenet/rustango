//! Form parsing, validation, and saving — shared between the auto-admin
//! and user route handlers.
//!
//! ## Three form types
//!
//! | Type | When to use |
//! |---|---|
//! | [`Form`] + `#[derive(Form)]` | Typed struct with declared fields and compile-time validators |
//! | [`ModelForm`] | Any [`Model`] table — parse + validate + save without a dedicated struct |
//! | [`DynamicForm`] | Runtime JSON-schema forms (surveys, intake, admin-configurable) |
//!
//! ## `#[derive(Form)]` usage
//!
//! ```ignore
//! use rustango::forms::Form;
//!
//! #[derive(Form)]
//! struct ContactForm {
//!     #[form(max_length = 100)]
//!     name: String,
//!     #[form(max_length = 200)]
//!     email: String,
//!     #[form(required = false)]
//!     message: Option<String>,
//! }
//!
//! // In a handler:
//! let form_data: HashMap<String, String> = /* axum Form extractor */;
//! match ContactForm::parse(&form_data) {
//!     Ok(form) => { /* use form.name, form.email, form.message */ }
//!     Err(errors) => { /* render errors */ }
//! }
//! ```
//!
//! ## `ModelForm` usage
//!
//! ```ignore
//! use rustango::forms::ModelForm;
//!
//! // Insert
//! let form = ModelForm::new(Post::SCHEMA, form_data);
//! match form.save(&pool).await {
//!     Ok(pk) => redirect(pk),
//!     Err(ModelFormError::Validation(errors)) => render_with_errors(errors),
//!     Err(ModelFormError::Database(e)) => server_error(e),
//! }
//!
//! // Update (provide the existing PK)
//! let pk = SqlValue::I64(post_id);
//! let form = ModelForm::for_update(Post::SCHEMA, form_data, pk);
//! form.save(&pool).await?;
//! ```

use std::collections::HashMap;

use crate::core::{
    Assignment, FieldSchema, FieldType, Filter, InsertQuery, ModelSchema, Op, SqlValue,
    UpdateQuery, WhereExpr,
};

#[cfg(feature = "csrf")]
pub mod csrf;

/// Form sets — Django's `formset_factory` / `modelformset_factory`
/// shape. Parse N copies of the same [`Form`] from a single
/// HTTP request payload keyed `<prefix>-<N>-<field>`. Issue #49.
pub mod formset;

/// Reusable declarative field-constraint validators (`max_length` /
/// `min_length` / `min` / `max` / `choices`), shared by the serializer
/// write path. Messages match the admin `DynamicForm`.
pub mod validators;

// ------------------------------------------------------------------ FormErrors

/// All validation errors collected from a form submission.
///
/// Field errors are keyed by the Rust field name. Non-field errors
/// are for cross-field or business-logic failures not tied to a
/// single input.
#[derive(Debug, Default, thiserror::Error)]
pub struct FormErrors {
    fields: HashMap<String, Vec<String>>,
    non_field: Vec<String>,
}

impl FormErrors {
    /// Add an error for a specific field.
    pub fn add(&mut self, field: impl Into<String>, msg: impl Into<String>) {
        self.fields
            .entry(field.into())
            .or_default()
            .push(msg.into());
    }

    /// Add an error not tied to any specific field.
    pub fn add_non_field(&mut self, msg: impl Into<String>) {
        self.non_field.push(msg.into());
    }

    /// `true` when there are no errors.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.non_field.is_empty()
    }

    /// Per-field error lists, keyed by field name.
    pub fn fields(&self) -> &HashMap<String, Vec<String>> {
        &self.fields
    }

    /// Non-field error list.
    pub fn non_field(&self) -> &[String] {
        &self.non_field
    }

    /// Drain every entry from `other` into `self`. Used by composable
    /// validation flows that aggregate per-field + cross-field errors
    /// into one collection. #436.
    pub fn merge(&mut self, mut other: FormErrors) {
        for (field, msgs) in other.fields.drain() {
            self.fields.entry(field).or_default().extend(msgs);
        }
        self.non_field.extend(other.non_field.drain(..));
    }

    /// All messages for `field`, or an empty slice.
    pub fn get(&self, field: &str) -> &[String] {
        self.fields.get(field).map(Vec::as_slice).unwrap_or(&[])
    }

    /// First field name with errors, if any. Useful for converting to
    /// single-error representations.
    pub fn first_field(&self) -> Option<&str> {
        self.fields.keys().next().map(String::as_str)
    }
}

impl std::fmt::Display for FormErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (field, msgs) in &self.fields {
            for msg in msgs {
                writeln!(f, "{field}: {msg}")?;
            }
        }
        for msg in &self.non_field {
            writeln!(f, "__all__: {msg}")?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ Form trait

/// Trait every `#[derive(Form)]` struct implements.
///
/// Parse a string-keyed payload (the shape axum's
/// `Form<HashMap<String, String>>` produces) into the typed struct,
/// collecting **all** field errors before returning.
pub trait Form: Sized {
    /// Parse a form payload. All field errors are collected — the
    /// returned `Err` may describe failures in more than one field.
    ///
    /// # Errors
    /// Returns [`FormErrors`] describing every field that failed
    /// validation.
    fn parse(data: &HashMap<String, String>) -> Result<Self, FormErrors>;
}

// ------------------------------------------------------------------ FormError (legacy single-error)

/// Single-field error type used by the low-level parsers and the admin.
///
/// New code should use [`FormErrors`] (multi-error). This type is kept
/// for the admin's CRUD path which reports one error at a time.
#[derive(Debug, thiserror::Error)]
pub enum FormError {
    #[error("required field `{field}` was missing from the form")]
    Missing { field: String },

    #[error("field `{field}` has invalid {ty} value `{value}`: {detail}")]
    Parse {
        field: String,
        ty: &'static str,
        value: String,
        detail: String,
    },

    #[error("PK field `{field}` of type {ty} is not supported in URL paths")]
    UnsupportedPk { field: String, ty: &'static str },
}

impl From<FormError> for FormErrors {
    fn from(e: FormError) -> Self {
        let mut errors = FormErrors::default();
        match &e {
            FormError::Missing { field } | FormError::Parse { field, .. } => {
                errors.add(field.clone(), e.to_string());
            }
            FormError::UnsupportedPk { field, .. } => {
                errors.add(field.clone(), e.to_string());
            }
        }
        errors
    }
}

// ------------------------------------------------------------------ low-level parsers (admin + ModelForm)

/// Parse a single PK fragment from a URL path segment into an `SqlValue`.
///
/// # Errors
/// [`FormError::Parse`] when the string doesn't match the field's type.
/// [`FormError::UnsupportedPk`] when the field type can't sit in a URL path.
pub fn parse_pk_string(field: &FieldSchema, raw: &str) -> Result<SqlValue, FormError> {
    let make_parse_err = |ty: &'static str, e: &dyn std::fmt::Display| FormError::Parse {
        field: field.name.to_owned(),
        ty,
        value: raw.to_owned(),
        detail: e.to_string(),
    };
    match field.ty {
        FieldType::I16 => raw
            .parse::<i16>()
            .map(SqlValue::I16)
            .map_err(|e| make_parse_err("i16", &e)),
        FieldType::I32 => raw
            .parse::<i32>()
            .map(SqlValue::I32)
            .map_err(|e| make_parse_err("i32", &e)),
        FieldType::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|e| make_parse_err("i64", &e)),
        FieldType::String => Ok(SqlValue::String(raw.to_owned())),
        FieldType::Uuid => uuid::Uuid::parse_str(raw)
            .map(SqlValue::Uuid)
            .map_err(|e| make_parse_err("Uuid", &e)),
        FieldType::Bool
        | FieldType::F32
        | FieldType::F64
        | FieldType::DateTime
        | FieldType::Date
        | FieldType::Time
        | FieldType::Json
        | FieldType::Decimal
        | FieldType::Binary
        // #341 / #343 / #342 / #824 / #443 — array / range / hstore /
        // vector / geometry can't be a PK.
        | FieldType::Array(_)
        | FieldType::Range(_)
        | FieldType::HStore
        | FieldType::Vector(_)
        | FieldType::Geometry(_) => Err(FormError::UnsupportedPk {
            field: field.name.to_owned(),
            ty: field.ty.as_str(),
        }),
    }
}

/// Parse one form value from a raw string.
///
/// Empty string + nullable field → `SqlValue::Null`.
/// Empty string + required field → `FormError::Missing`.
/// Bool fields treat absent key as `false` (unchecked checkbox).
///
/// # Errors
/// As [`parse_pk_string`], plus [`FormError::Missing`].
pub fn parse_form_value(field: &FieldSchema, raw: Option<&str>) -> Result<SqlValue, FormError> {
    let Some(raw) = raw else {
        return Ok(match field.ty {
            FieldType::Bool => SqlValue::Bool(false),
            _ if field.nullable => SqlValue::Null,
            _ => {
                return Err(FormError::Missing {
                    field: field.name.to_owned(),
                });
            }
        });
    };
    if field.nullable && raw.is_empty() {
        return Ok(SqlValue::Null);
    }
    // Non-nullable String field with empty raw is a *missing* value,
    // not a valid empty string — matches Django/DRF where CharField
    // rejects "" unless allow_blank=True. Without this guard, blank
    // form submits silently land empty strings in NOT NULL columns
    // (surfaced playing with the cookbook /authors/new form).
    if matches!(field.ty, FieldType::String) && !field.nullable && raw.is_empty() {
        return Err(FormError::Missing {
            field: field.name.to_owned(),
        });
    }
    let make_parse_err = |ty: &'static str, e: &dyn std::fmt::Display| FormError::Parse {
        field: field.name.to_owned(),
        ty,
        value: raw.to_owned(),
        detail: e.to_string(),
    };
    match field.ty {
        FieldType::Bool => {
            let v = !matches!(
                raw.to_ascii_lowercase().as_str(),
                "" | "false" | "0" | "off" | "no"
            );
            Ok(SqlValue::Bool(v))
        }
        FieldType::I16 => raw
            .parse::<i16>()
            .map(SqlValue::I16)
            .map_err(|e| make_parse_err("i16", &e)),
        FieldType::I32 => raw
            .parse::<i32>()
            .map(SqlValue::I32)
            .map_err(|e| make_parse_err("i32", &e)),
        FieldType::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|e| make_parse_err("i64", &e)),
        FieldType::F32 => raw
            .parse::<f32>()
            .map(SqlValue::F32)
            .map_err(|e| make_parse_err("f32", &e)),
        FieldType::F64 => raw
            .parse::<f64>()
            .map(SqlValue::F64)
            .map_err(|e| make_parse_err("f64", &e)),
        FieldType::String => Ok(SqlValue::String(raw.to_owned())),
        FieldType::Uuid => uuid::Uuid::parse_str(raw)
            .map(SqlValue::Uuid)
            .map_err(|e| make_parse_err("Uuid", &e)),
        FieldType::Date => chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .map(SqlValue::Date)
            .map_err(|e| make_parse_err("Date", &e)),
        FieldType::DateTime => {
            if let Ok(d) = chrono::DateTime::parse_from_rfc3339(raw) {
                return Ok(SqlValue::DateTime(d.with_timezone(&chrono::Utc)));
            }
            let ndt = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M"))
                .map_err(|e| make_parse_err("DateTime", &e))?;
            Ok(SqlValue::DateTime(ndt.and_utc()))
        }
        FieldType::Json => {
            if raw.is_empty() {
                Ok(SqlValue::Json(serde_json::json!({})))
            } else {
                serde_json::from_str::<serde_json::Value>(raw)
                    .map(SqlValue::Json)
                    .map_err(|e| FormError::Parse {
                        field: field.name.to_owned(),
                        ty: "Json",
                        value: raw.to_owned(),
                        detail: e.to_string(),
                    })
            }
        }
        // Decimal accepts standard `123.45` / `-0.001` / `1e3` forms via
        // `rust_decimal::Decimal::from_str_exact`; reject anything else
        // rather than silently truncate. Django's DecimalField behaves
        // the same way.
        FieldType::Decimal => raw
            .parse::<rust_decimal::Decimal>()
            .map(SqlValue::Decimal)
            .map_err(|e| make_parse_err("Decimal", &e)),
        // Binary form input is uncommon; accept lowercase hex (no
        // separator, even-length) and reject anything else. File
        // uploads / base64 / multipart are a separate code path.
        FieldType::Binary => {
            if raw.len() % 2 != 0 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(make_parse_err(
                    "Binary",
                    &"expected lowercase hex (even length)",
                ));
            }
            let bytes = raw
                .as_bytes()
                .chunks_exact(2)
                .map(|c| {
                    let h = (c[0] as char).to_digit(16).unwrap_or(0) as u8;
                    let l = (c[1] as char).to_digit(16).unwrap_or(0) as u8;
                    (h << 4) | l
                })
                .collect::<Vec<u8>>();
            Ok(SqlValue::Binary(bytes))
        }
        FieldType::Time => chrono::NaiveTime::parse_from_str(raw, "%H:%M:%S")
            .or_else(|_| chrono::NaiveTime::parse_from_str(raw, "%H:%M"))
            .map(SqlValue::Time)
            .map_err(|e| make_parse_err("Time", &e)),
        // #341 — PG array column from a form field: comma-separated
        // input (`a, b, c`). Empty / whitespace-only input → empty array.
        FieldType::Array(elem) => {
            let parts: Vec<&str> = if raw.trim().is_empty() {
                Vec::new()
            } else {
                raw.split(',').map(str::trim).collect()
            };
            match elem {
                crate::core::ArrayElem::Text => Ok(SqlValue::Array(
                    parts
                        .into_iter()
                        .map(|s| SqlValue::String(s.to_owned()))
                        .collect(),
                )),
                crate::core::ArrayElem::Int => parts
                    .into_iter()
                    .map(|s| s.parse::<i32>().map(SqlValue::I32))
                    .collect::<Result<Vec<_>, _>>()
                    .map(SqlValue::Array)
                    .map_err(|e| make_parse_err("array<i32>", &e)),
                crate::core::ArrayElem::BigInt => parts
                    .into_iter()
                    .map(|s| s.parse::<i64>().map(SqlValue::I64))
                    .collect::<Result<Vec<_>, _>>()
                    .map(SqlValue::Array)
                    .map_err(|e| make_parse_err("array<i64>", &e)),
            }
        }
        // #343 — PG range column from a form field: the raw input is a
        // range literal (`[1,10)`), bound as-is and implicit-cast by PG.
        FieldType::Range(_) => Ok(SqlValue::RangeLiteral(raw.to_owned())),
        // #342 — PG hstore column from a form field: a JSON object
        // (`{"k":"v"}`, null values allowed) parsed into key→value pairs.
        FieldType::HStore => {
            let map: std::collections::BTreeMap<String, Option<String>> = serde_json::from_str(raw)
                .map_err(|e| make_parse_err("hstore (JSON object)", &e))?;
            Ok(SqlValue::HStore(map.into_iter().collect()))
        }
        // #824 — pgvector column from a form field: a JSON array of
        // numbers (`[0.1, 0.2, 0.3]`).
        FieldType::Vector(_) => {
            let vec: Vec<f32> = serde_json::from_str(raw)
                .map_err(|e| make_parse_err("vector (JSON array of numbers)", &e))?;
            Ok(SqlValue::Vector(vec))
        }
        // #443 — PostGIS geometry from a form field: a JSON Point object
        // (`{"x": 1.5, "y": -2.25, "srid": 4326}`; `srid` defaults to 4326).
        FieldType::Geometry(_) => {
            let p: crate::sql::Point = serde_json::from_str(raw)
                .map_err(|e| make_parse_err("geometry (JSON {x, y, srid?})", &e))?;
            Ok(p.into())
        }
    }
}

/// Walk every scalar field of `model` and turn the form payload into
/// a `(column, value)` list ready to feed an `InsertQuery` /
/// `UpdateQuery`. `skip` is a list of field names to omit.
///
/// # Errors
/// As [`parse_form_value`].
pub fn collect_values(
    model: &'static ModelSchema,
    form: &HashMap<String, String>,
    skip: &[&str],
) -> Result<Vec<(&'static str, SqlValue)>, FormError> {
    let mut out = Vec::new();
    for field in model.scalar_fields() {
        // Server-assigned columns (`Auto<T>` PK with BIGSERIAL,
        // `auto_now_add` / `auto_now` mixins, `auto_uuid`) are never
        // present in HTML forms — the macro skips them on INSERT and
        // the DB DEFAULT supplies the value. Filtering them here
        // keeps both code paths in lock-step. See cookbook chapter 7
        // `modelform_parses_form_encoded_into_typed_values` for the
        // ModelFormFor analogue.
        if field.auto || skip.contains(&field.name) {
            continue;
        }
        let raw = form.get(field.name).map(String::as_str);
        let value = parse_form_value(field, raw)?;
        out.push((field.column, value));
    }
    Ok(out)
}

// ------------------------------------------------------------------ ModelForm

/// Error returned by [`ModelForm::save`].
#[derive(Debug, thiserror::Error)]
pub enum ModelFormError {
    #[error("form validation failed:\n{0}")]
    Validation(FormErrors),
    #[error("database error: {0}")]
    Database(#[from] crate::sql::ExecError),
}

/// Schema-driven form that can insert or update any [`Model`] row.
///
/// `ModelForm` reads the model's [`ModelSchema`] to know which fields
/// to parse and validate — no separate struct required.
///
/// ## Insert
///
/// ```ignore
/// let form = ModelForm::new(Post::SCHEMA, form_data);
/// match form.save(&pool).await {
///     Ok(pk) => { /* redirect to /__admin/post/{pk} */ }
///     Err(ModelFormError::Validation(errors)) => { /* render errors */ }
///     Err(ModelFormError::Database(e)) => { /* 500 */ }
/// }
/// ```
///
/// ## Update
///
/// ```ignore
/// let pk = SqlValue::I64(post_id);
/// let form = ModelForm::for_update(Post::SCHEMA, form_data, pk);
/// form.save(&pool).await?;
/// ```
pub struct ModelForm {
    schema: &'static ModelSchema,
    data: HashMap<String, String>,
    pk_value: Option<SqlValue>,
    include_fields: Option<Vec<String>>,
    exclude_fields: Vec<String>,
}

impl ModelForm {
    /// Create a form for **inserting** a new row.
    pub fn new(schema: &'static ModelSchema, data: HashMap<String, String>) -> Self {
        Self {
            schema,
            data,
            pk_value: None,
            include_fields: None,
            exclude_fields: Vec::new(),
        }
    }

    /// Create a form for **updating** the row identified by `pk`.
    pub fn for_update(
        schema: &'static ModelSchema,
        data: HashMap<String, String>,
        pk: SqlValue,
    ) -> Self {
        Self {
            schema,
            data,
            pk_value: Some(pk),
            include_fields: None,
            exclude_fields: Vec::new(),
        }
    }

    /// Restrict the form to only the named fields. By default all
    /// non-PK, non-auto scalar fields are included.
    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.include_fields = Some(fields.iter().map(|&s| s.to_owned()).collect());
        self
    }

    /// Drop the named fields from the form. v0.49 — Django's
    /// `Meta.exclude` analog. Applied AFTER `fields(...)` if both
    /// are set, so `.fields(&["a", "b", "c"]).exclude(&["b"])`
    /// produces `["a", "c"]`. Excluding a field also drops it from
    /// validation / INSERT / UPDATE; PK / auto fields are excluded
    /// unconditionally regardless of this list.
    pub fn exclude(mut self, fields: &[&str]) -> Self {
        for f in fields {
            self.exclude_fields.push((*f).to_owned());
        }
        self
    }

    fn should_include(&self, field: &FieldSchema) -> bool {
        if field.primary_key || field.auto {
            return false;
        }
        if self.exclude_fields.iter().any(|n| n == field.name) {
            return false;
        }
        match &self.include_fields {
            Some(list) => list.iter().any(|n| n == field.name),
            None => true,
        }
    }

    /// v0.49 — test-only accessor returning the field NAMES the form
    /// currently includes (after applying `fields(...)` /
    /// `exclude(...)` and skipping PK / auto). Useful for asserting
    /// the builder semantics without driving a full validate/save.
    #[cfg(test)]
    pub(crate) fn included_field_names(&self) -> Vec<&'static str> {
        self.schema
            .scalar_fields()
            .filter(|f| self.should_include(f))
            .map(|f| f.name)
            .collect()
    }

    /// Validate form data against the schema. Returns all errors.
    pub fn validate(&self) -> FormErrors {
        let mut errors = FormErrors::default();
        for field in self.schema.scalar_fields() {
            if !self.should_include(field) {
                continue;
            }
            let raw = self.data.get(field.name).map(String::as_str);
            if let Err(e) = parse_form_value(field, raw) {
                errors.add(field.name, e.to_string());
            }
        }
        errors
    }

    /// `true` when all fields pass validation.
    pub fn is_valid(&self) -> bool {
        self.validate().is_empty()
    }

    /// All validation errors. Equivalent to calling `validate()`.
    pub fn errors(&self) -> FormErrors {
        self.validate()
    }

    /// Validate and execute the INSERT or UPDATE. Returns the PK value
    /// (newly generated for inserts; the supplied value for updates).
    ///
    /// v0.38 — fully tri-dialect via `&crate::sql::Pool`. Routes through
    /// the backend-erasing `update_pool` / `insert_returning_pool`
    /// helpers and decodes the returned PK per backend (PgRow on PG,
    /// `LAST_INSERT_ID()` on MySQL, SqliteRow on SQLite).
    ///
    /// # Errors
    /// [`ModelFormError::Validation`] if any field is invalid.
    /// [`ModelFormError::Database`] for driver-level failures.
    pub async fn save(&self, pool: &crate::sql::Pool) -> Result<SqlValue, ModelFormError> {
        self.prepare_save()?.commit_pool(pool).await
    }

    /// Django-shape `form.save(commit=False)` — issue #375. Validates
    /// every included field and returns a mutable
    /// [`PreparedSave`] holding the parsed columns + values, without
    /// touching the DB. The caller can `.set(column, value)` to add
    /// fields the form didn't have (e.g. `author_id` derived from the
    /// session) or override parsed values, then call
    /// [`PreparedSave::commit_pool`] to actually run the
    /// INSERT or UPDATE.
    ///
    /// ```ignore
    /// let mut prep = form.prepare_save()?;
    /// prep.set("author_id", SqlValue::I64(request_user_id));
    /// let pk = prep.commit_pool(&pool).await?;
    /// ```
    ///
    /// # Errors
    /// [`ModelFormError::Validation`] if any field is invalid.
    /// [`ModelFormError::Database`] if the model has no primary key.
    pub fn prepare_save(&self) -> Result<PreparedSave, ModelFormError> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(ModelFormError::Validation(errors));
        }

        let pk_field = self.schema.primary_key().ok_or_else(|| {
            ModelFormError::Database(crate::sql::ExecError::Driver(sqlx::Error::Protocol(
                "model has no primary key".into(),
            )))
        })?;

        let mut columns: Vec<&'static str> = Vec::new();
        let mut values: Vec<SqlValue> = Vec::new();
        for field in self.schema.scalar_fields() {
            if !self.should_include(field) {
                continue;
            }
            let raw = self.data.get(field.name).map(String::as_str);
            if let Ok(v) = parse_form_value(field, raw) {
                columns.push(field.column);
                values.push(v);
            }
        }

        Ok(PreparedSave {
            schema: self.schema,
            pk_field,
            pk_value: self.pk_value.clone(),
            columns,
            values,
        })
    }
}

/// Result of `form.prepare_save()` — issue #375 / Django
/// `form.save(commit=False)`. Holds the validated columns + values
/// ready to INSERT or UPDATE; caller can mutate before
/// [`Self::commit_pool`] to add session-derived fields the form
/// didn't expose.
///
/// `save_m2m()` — Django's deferred M2M companion — has no analog
/// yet because rustango's `ModelForm` doesn't surface M2M form
/// fields; once it does, the deferred-apply lives on this struct.
#[derive(Debug, Clone)]
pub struct PreparedSave {
    schema: &'static ModelSchema,
    pk_field: &'static FieldSchema,
    /// `Some` for UPDATEs (carried over from `ModelForm::pk_value`),
    /// `None` for INSERTs.
    pk_value: Option<SqlValue>,
    /// Columns about to be written, parallel to `values`. Mutable
    /// via [`Self::set`] / [`Self::unset`].
    columns: Vec<&'static str>,
    /// Parsed values for each column, parallel to `columns`.
    values: Vec<SqlValue>,
}

impl PreparedSave {
    /// Add a column / value pair, or replace it if one already
    /// exists for that name. Column lookup is by `FieldSchema.name`
    /// (the Rust field ident, not the SQL column) so callers can use
    /// the same names they wrote in the model definition. Returns
    /// `&mut Self` for chaining.
    ///
    /// Use this when the form omits a field that the DB needs (e.g.
    /// `author_id` derived from the request user) or to override a
    /// parsed value before commit.
    ///
    /// Unknown field names are a no-op — callers that care can
    /// inspect [`Self::columns`] / [`Self::has`] first.
    pub fn set(&mut self, field: &str, value: impl Into<SqlValue>) -> &mut Self {
        let Some(target_col) = self
            .schema
            .scalar_fields()
            .find(|f| f.name == field)
            .map(|f| f.column)
        else {
            return self;
        };
        if let Some(idx) = self.columns.iter().position(|c| *c == target_col) {
            self.values[idx] = value.into();
        } else {
            self.columns.push(target_col);
            self.values.push(value.into());
        }
        self
    }

    /// Drop a column from the prepared write. Mirrors Django's
    /// `del obj.field` between `save(commit=False)` and `obj.save()`.
    /// Unknown field names are a no-op.
    pub fn unset(&mut self, field: &str) -> &mut Self {
        let Some(target_col) = self
            .schema
            .scalar_fields()
            .find(|f| f.name == field)
            .map(|f| f.column)
        else {
            return self;
        };
        if let Some(idx) = self.columns.iter().position(|c| *c == target_col) {
            self.columns.remove(idx);
            self.values.remove(idx);
        }
        self
    }

    /// `true` when the named field is currently in the prepared
    /// write set. Useful for caller-side branching before
    /// [`Self::commit_pool`].
    #[must_use]
    pub fn has(&self, field: &str) -> bool {
        self.schema
            .scalar_fields()
            .find(|f| f.name == field)
            .is_some_and(|f| self.columns.iter().any(|c| *c == f.column))
    }

    /// `true` when this prepared save will INSERT, `false` when it
    /// will UPDATE an existing row. Mirrors the form-side `pk_value`
    /// discriminant.
    #[must_use]
    pub fn is_insert(&self) -> bool {
        self.pk_value.is_none()
    }

    /// Execute the actual INSERT or UPDATE. Same return shape as
    /// [`ModelForm::save`] — the new PK on INSERT, the supplied PK
    /// on UPDATE.
    ///
    /// # Errors
    /// [`ModelFormError::Database`] for driver-level failures.
    pub async fn commit_pool(self, pool: &crate::sql::Pool) -> Result<SqlValue, ModelFormError> {
        if let Some(pk_val) = self.pk_value {
            let assignments: Vec<Assignment> = self
                .columns
                .iter()
                .zip(self.values)
                .map(|(col, val)| Assignment {
                    column: col,
                    value: val.into(),
                })
                .collect();
            let query = UpdateQuery {
                model: self.schema,
                set: assignments,
                where_clause: WhereExpr::Predicate(Filter {
                    column: self.pk_field.column,
                    op: Op::Eq,
                    value: pk_val.clone(),
                }),
            };
            crate::sql::update_pool(pool, &query).await?;
            return Ok(pk_val);
        }

        let query = InsertQuery {
            model: self.schema,
            columns: self.columns,
            values: self.values,
            returning: vec![self.pk_field.column],
            on_conflict: None,
        };
        let returning = crate::sql::insert_returning_pool(pool, &query).await?;
        let pk_val: SqlValue = match returning {
            #[cfg(feature = "postgres")]
            crate::sql::InsertReturningPool::PgRow(row) => {
                use crate::sql::sqlx::Row as _;
                match self.pk_field.ty {
                    FieldType::I64 => SqlValue::I64(row.try_get(self.pk_field.column).unwrap_or(0)),
                    FieldType::I32 => SqlValue::I32(row.try_get(self.pk_field.column).unwrap_or(0)),
                    FieldType::I16 => SqlValue::I16(row.try_get(self.pk_field.column).unwrap_or(0)),
                    FieldType::String => {
                        SqlValue::String(row.try_get(self.pk_field.column).unwrap_or_default())
                    }
                    _ => SqlValue::Null,
                }
            }
            #[cfg(feature = "mysql")]
            crate::sql::InsertReturningPool::MySqlAutoId(id) => match self.pk_field.ty {
                FieldType::I64 => SqlValue::I64(id),
                FieldType::I32 => SqlValue::I32(id as i32),
                FieldType::I16 => SqlValue::I16(id as i16),
                _ => SqlValue::I64(id),
            },
            #[cfg(feature = "sqlite")]
            crate::sql::InsertReturningPool::SqliteRow(row) => {
                use crate::sql::sqlx::Row as _;
                match self.pk_field.ty {
                    FieldType::I64 => SqlValue::I64(row.try_get(self.pk_field.column).unwrap_or(0)),
                    FieldType::I32 => SqlValue::I32(row.try_get(self.pk_field.column).unwrap_or(0)),
                    FieldType::I16 => SqlValue::I16(row.try_get(self.pk_field.column).unwrap_or(0)),
                    FieldType::String => {
                        SqlValue::String(row.try_get(self.pk_field.column).unwrap_or_default())
                    }
                    _ => SqlValue::Null,
                }
            }
        };
        Ok(pk_val)
    }
}

// ------------------------------------------------------------------ DynamicForm

/// Field types supported in a [`DynamicForm`] schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicFieldType {
    Text,
    Textarea,
    Integer,
    Float,
    Boolean,
    Date,
    Datetime,
    Email,
    Url,
    Select,
    MultiSelect,
}

/// One field descriptor in a [`DynamicForm`].
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DynamicField {
    /// Field name — used as the form input name and the cleaned-data key.
    pub name: String,
    /// Human-readable label shown next to the input.
    #[serde(default)]
    pub label: String,
    pub field_type: DynamicFieldType,
    #[serde(default = "bool_true")]
    pub required: bool,
    pub max_length: Option<usize>,
    pub min_length: Option<usize>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `[(value, display_label)]` pairs — required for `Select` /
    /// `MultiSelect` fields.
    #[serde(default)]
    pub choices: Vec<(String, String)>,
    /// Help text shown below the input.
    #[serde(default)]
    pub help_text: String,
}

fn bool_true() -> bool {
    true
}

/// Runtime JSON-schema driven form.
///
/// Build a form from a JSON array of field descriptors (useful for
/// survey-style forms configured by operators at runtime), bind POST
/// data to it, validate, and read the cleaned values.
///
/// ## Schema format
///
/// ```json
/// [
///   {"name": "title",    "field_type": "text",    "required": true, "max_length": 200},
///   {"name": "rating",   "field_type": "integer",  "required": true, "min": 1, "max": 5},
///   {"name": "category", "field_type": "select",   "required": true,
///    "choices": [["tech", "Technology"], ["news", "News"]]},
///   {"name": "notes",    "field_type": "textarea", "required": false}
/// ]
/// ```
///
/// ## Usage
///
/// ```ignore
/// let schema = serde_json::from_str(schema_json)?;
/// let mut form = DynamicForm::from_schema(schema);
/// form.bind(form_data);
/// if form.is_valid() {
///     let data = form.cleaned_data().unwrap();
///     println!("{}", data["title"]);
/// }
/// ```
pub struct DynamicForm {
    fields: Vec<DynamicField>,
    data: Option<HashMap<String, String>>,
}

impl DynamicForm {
    /// Build a form from a pre-parsed list of field descriptors.
    pub fn from_schema(fields: Vec<DynamicField>) -> Self {
        Self { fields, data: None }
    }

    /// Build a form from a JSON value (`serde_json::Value::Array`).
    ///
    /// # Errors
    /// [`serde_json::Error`] when the JSON doesn't match the schema format.
    pub fn from_json(schema: serde_json::Value) -> Result<Self, serde_json::Error> {
        let fields: Vec<DynamicField> = serde_json::from_value(schema)?;
        Ok(Self::from_schema(fields))
    }

    /// Bind a form payload (typically from `axum::Form<HashMap<...>>`).
    pub fn bind(&mut self, data: HashMap<String, String>) {
        self.data = Some(data);
    }

    /// The field descriptors for this form (useful for template rendering).
    pub fn fields(&self) -> &[DynamicField] {
        &self.fields
    }

    /// Validate the bound data. Returns all errors.
    ///
    /// Returns empty `FormErrors` when the form is not yet bound.
    pub fn validate(&self) -> FormErrors {
        let mut errors = FormErrors::default();
        let Some(data) = &self.data else {
            return errors;
        };

        for field in &self.fields {
            let raw = data.get(&field.name).map(String::as_str);

            match raw {
                None | Some("")
                    if field.required && field.field_type != DynamicFieldType::Boolean =>
                {
                    errors.add(&field.name, "This field is required.");
                    continue;
                }
                _ => {}
            }

            let raw_str = raw.unwrap_or("");

            match field.field_type {
                DynamicFieldType::Integer => {
                    if !raw_str.is_empty() {
                        match raw_str.parse::<i64>() {
                            Ok(n) => {
                                if let Some(min) = field.min {
                                    if (n as f64) < min {
                                        errors.add(
                                            &field.name,
                                            format!("Ensure this value is ≥ {min}."),
                                        );
                                    }
                                }
                                if let Some(max) = field.max {
                                    if (n as f64) > max {
                                        errors.add(
                                            &field.name,
                                            format!("Ensure this value is ≤ {max}."),
                                        );
                                    }
                                }
                            }
                            Err(_) => errors.add(&field.name, "Enter a whole number."),
                        }
                    }
                }
                DynamicFieldType::Float => {
                    if !raw_str.is_empty() {
                        match raw_str.parse::<f64>() {
                            Ok(n) => {
                                if let Some(min) = field.min {
                                    if n < min {
                                        errors.add(
                                            &field.name,
                                            format!("Ensure this value is ≥ {min}."),
                                        );
                                    }
                                }
                                if let Some(max) = field.max {
                                    if n > max {
                                        errors.add(
                                            &field.name,
                                            format!("Ensure this value is ≤ {max}."),
                                        );
                                    }
                                }
                            }
                            Err(_) => errors.add(&field.name, "Enter a number."),
                        }
                    }
                }
                DynamicFieldType::Text
                | DynamicFieldType::Textarea
                | DynamicFieldType::Email
                | DynamicFieldType::Url => {
                    if let Some(max) = field.max_length {
                        if raw_str.len() > max {
                            errors.add(
                                &field.name,
                                format!("Ensure this value has at most {max} characters."),
                            );
                        }
                    }
                    if let Some(min) = field.min_length {
                        if !raw_str.is_empty() && raw_str.len() < min {
                            errors.add(
                                &field.name,
                                format!("Ensure this value has at least {min} characters."),
                            );
                        }
                    }
                    if field.field_type == DynamicFieldType::Email && !raw_str.is_empty() {
                        if !raw_str.contains('@') {
                            errors.add(&field.name, "Enter a valid email address.");
                        }
                    }
                }
                DynamicFieldType::Select => {
                    if !raw_str.is_empty() && !field.choices.iter().any(|(v, _)| v == raw_str) {
                        errors.add(&field.name, "Select a valid choice.");
                    }
                }
                DynamicFieldType::MultiSelect => {
                    // multi-select values are comma-separated
                    for part in raw_str.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        if !field.choices.iter().any(|(v, _)| v == part) {
                            errors.add(&field.name, format!("'{part}' is not a valid choice."));
                        }
                    }
                }
                DynamicFieldType::Date => {
                    if !raw_str.is_empty() {
                        if chrono::NaiveDate::parse_from_str(raw_str, "%Y-%m-%d").is_err() {
                            errors.add(&field.name, "Enter a valid date (YYYY-MM-DD).");
                        }
                    }
                }
                DynamicFieldType::Datetime => {
                    if !raw_str.is_empty() {
                        let ok = chrono::DateTime::parse_from_rfc3339(raw_str).is_ok()
                            || chrono::NaiveDateTime::parse_from_str(raw_str, "%Y-%m-%dT%H:%M:%S")
                                .is_ok()
                            || chrono::NaiveDateTime::parse_from_str(raw_str, "%Y-%m-%dT%H:%M")
                                .is_ok();
                        if !ok {
                            errors.add(&field.name, "Enter a valid date/time.");
                        }
                    }
                }
                DynamicFieldType::Boolean => {}
            }
        }
        errors
    }

    /// `true` when the form is bound and all fields pass validation.
    pub fn is_valid(&self) -> bool {
        self.data.is_some() && self.validate().is_empty()
    }

    /// All validation errors for the currently bound data.
    pub fn errors(&self) -> FormErrors {
        self.validate()
    }

    /// Return cleaned (parsed) values for all fields.
    ///
    /// # Errors
    /// Returns [`FormErrors`] if validation fails. Call [`is_valid`][Self::is_valid]
    /// first if you want to inspect errors separately.
    pub fn cleaned_data(&self) -> Result<HashMap<String, serde_json::Value>, FormErrors> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(errors);
        }
        let data = self.data.as_ref().map_or_else(HashMap::new, Clone::clone);
        let mut out = HashMap::new();
        for field in &self.fields {
            let raw = data.get(&field.name).map(String::as_str).unwrap_or("");
            let value = match field.field_type {
                DynamicFieldType::Integer => {
                    if raw.is_empty() {
                        serde_json::Value::Null
                    } else {
                        let n = raw.parse::<i64>().unwrap_or(0);
                        serde_json::Value::Number(serde_json::Number::from(n))
                    }
                }
                DynamicFieldType::Float => {
                    if raw.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(raw.parse::<f64>().unwrap_or(0.0))
                    }
                }
                DynamicFieldType::Boolean => serde_json::Value::Bool(!matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "" | "false" | "0" | "off" | "no"
                )),
                DynamicFieldType::MultiSelect => {
                    let parts: Vec<serde_json::Value> = raw
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| serde_json::Value::String(s.to_owned()))
                        .collect();
                    serde_json::Value::Array(parts)
                }
                _ => {
                    if raw.is_empty() && !field.required {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(raw.to_owned())
                    }
                }
            };
            out.insert(field.name.clone(), value);
        }
        Ok(out)
    }
}

// ============================================================ ModelForm (v0.17.0 J)

/// Runtime ModelForm — parse a string-keyed payload into a typed
/// `(columns, values)` pair against a `T: Model` schema, ready to feed
/// straight into `sql::insert` / `sql::update` / their `_pool` variants.
///
/// The proc-macro `#[derive(ModelForm)]` follows in J.b; this is the
/// runtime engine it'll defer to. End-user shape today:
///
/// ```ignore
/// // POST /posts { title: "hello", body: "world" }
/// let mf: ModelFormFor<Post> = ModelForm::parse(&form_payload)?;
/// let query = mf.into_insert_query();
/// rustango::sql::insert_on(&pool, &query).await?;
/// ```
///
/// Skips the `Auto<T>` PK on create; rejects unknown form keys with
/// a clear error so callers can surface "you sent us a field we
/// don't recognise" instead of silently dropping data.
#[derive(Debug)]
pub struct ModelFormFor<T: crate::core::Model> {
    /// Column names in the same declaration order as the model
    /// schema (sans Auto<T> PK on create).
    columns: Vec<&'static str>,
    /// Parsed `SqlValue`s, parallel to `columns`.
    values: Vec<crate::core::SqlValue>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: crate::core::Model> ModelFormFor<T> {
    /// Parse + validate a JSON object against `T::SCHEMA`. Convenience
    /// over [`Self::parse`] for apps with JSON request bodies (most
    /// REST handlers): walks the JSON object, stringifies each value,
    /// and dispatches to the same per-field parser the form-encoded
    /// path uses.
    ///
    /// JSON null + missing keys map to absent (the field's nullable
    /// rule decides whether that's an error). Strings pass through
    /// directly; numbers/bools/Arrays/objects get JSON-stringified
    /// then parsed by `parse_form_value` via the field's declared
    /// type — so an i64 field accepts both `42` and `"42"`, and a
    /// JSON field accepts an inline object that gets re-stringified
    /// and re-parsed.
    ///
    /// # Errors
    /// As [`Self::parse`], plus `FormErrors` with a non-field error
    /// when `value` isn't a JSON object at the top level.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, FormErrors> {
        let obj = match value.as_object() {
            Some(o) => o,
            None => {
                let mut errors = FormErrors::default();
                errors.add_non_field("expected JSON object body");
                return Err(errors);
            }
        };
        let mut payload: HashMap<String, String> = HashMap::with_capacity(obj.len());
        for (k, v) in obj {
            // serde_json::Value -> String for parse_form_value's
            // string-keyed contract:
            //   - null  → absent (skip insert; field's nullable
            //             rule decides whether parse errors)
            //   - String→ raw inner value (no JSON quotes)
            //   - other → compact JSON serialization (numbers,
            //             bools, arrays, objects)
            match v {
                serde_json::Value::Null => {}
                serde_json::Value::String(s) => {
                    payload.insert(k.clone(), s.clone());
                }
                other => {
                    payload.insert(k.clone(), other.to_string());
                }
            }
        }
        Self::parse(&payload)
    }

    /// Parse + validate `payload` against `T::SCHEMA`. Walks every
    /// scalar field, runs `parse_form_value` per field (which honours
    /// `nullable` + per-type type checks), validates against
    /// `min` / `max` / `max_length` bounds.
    ///
    /// `Auto<T>` PK fields are skipped — the database assigns those
    /// on insert. Other PK fields (manual `i64`, `String`, etc.) are
    /// included; ModelForm treats them as ordinary required fields.
    ///
    /// # Errors
    /// [`FormErrors`] aggregating every per-field failure (missing,
    /// parse, bound-violation). Multi-error: every issue is reported
    /// together so callers can render a complete error summary.
    pub fn parse(payload: &HashMap<String, String>) -> Result<Self, FormErrors> {
        let mut errors = FormErrors::default();
        let mut columns = Vec::new();
        let mut values = Vec::new();
        for field in T::SCHEMA.scalar_fields() {
            // Skip fields the database fills in automatically:
            //   * Auto<T> PK (BIGSERIAL / SERIAL / gen_random_uuid())
            //   * `auto = true` non-PK fields (auto_now_add /
            //     auto_now mixins) — DB DEFAULT NOW() supplies the
            //     value on INSERT, and the macro skips them on the
            //     INSERT path too. ModelFormFor used to require these
            //     on every create, breaking any model with a created_at
            //     auto-timestamp.
            if field.auto {
                continue;
            }
            let raw = payload.get(field.name).map(String::as_str);
            match parse_form_value(field, raw) {
                Ok(value) => {
                    if let Err(bound_err) =
                        crate::core::validate_value(T::SCHEMA.table, field, &value)
                    {
                        errors.add(field.name.to_owned(), bound_err.to_string());
                    } else {
                        columns.push(field.column);
                        values.push(value);
                    }
                }
                Err(e) => {
                    errors.add(field.name.to_owned(), e.to_string());
                }
            }
        }
        if errors.is_empty() {
            Ok(Self {
                columns,
                values,
                _marker: std::marker::PhantomData,
            })
        } else {
            Err(errors)
        }
    }

    /// Borrow the parsed `(column, value)` pairs without consuming.
    /// Useful for tests + custom dispatch paths that don't want to
    /// build a full `InsertQuery`.
    #[must_use]
    pub fn columns(&self) -> &[&'static str] {
        &self.columns
    }

    /// Borrow the parsed values.
    #[must_use]
    pub fn values(&self) -> &[crate::core::SqlValue] {
        &self.values
    }

    // (helper for validate_unique_together below)

    /// DRF-shape `UniqueTogetherValidator` — pre-checks every composite
    /// UNIQUE index declared on `T::SCHEMA.indexes` (via
    /// `#[rustango(unique_together = "...")]`) by SELECT-ing the
    /// matching `(col1, col2, ...)` pair from the DB. Hits become
    /// per-field `FormErrors` keyed by *each* column in the conflicting
    /// tuple — a friendly alternative to the raw Postgres
    /// `duplicate key value violates unique constraint "..."` error.
    ///
    /// Pass the optional `pk_value` when validating an UPDATE so the
    /// row being edited isn't its own conflict (analog to DRF's
    /// `instance` parameter on the validator).
    ///
    /// v0.38 — tri-dialect via `&crate::sql::Pool`. Identifier quoting
    /// routes through `dialect.quote_ident` (double-quotes on PG/SQLite,
    /// backticks on MySQL) and placeholders through
    /// `dialect.placeholder(n)` (`$N` on PG, `?` on sqlite/mysql).
    ///
    /// # Errors
    /// Returns the accumulated [`FormErrors`] when any composite
    /// UNIQUE check finds a conflicting row in the DB. Driver / SQL
    /// failures land as a non-field error.
    pub async fn validate_unique_together(
        &self,
        pool: &crate::sql::Pool,
        pk_value: Option<&crate::core::SqlValue>,
    ) -> Result<(), FormErrors> {
        let mut errors = FormErrors::default();
        let pk_field = T::SCHEMA.primary_key();
        let dialect = pool.dialect();
        for idx in T::SCHEMA.indexes {
            if !idx.unique || idx.columns.len() < 2 {
                continue;
            }
            // Resolve `(column, value)` pairs from this form for every
            // column in the composite index. If any column is absent
            // (skipped, missing) we can't pre-check — let the DB
            // surface the conflict.
            let mut bound: Vec<(&'static str, crate::core::SqlValue)> = Vec::new();
            let mut all_present = true;
            for col in idx.columns {
                match self.columns.iter().position(|c| c == col) {
                    Some(i) => bound.push((
                        idx.columns.iter().find(|c| c == &col).copied().unwrap(),
                        self.values[i].clone(),
                    )),
                    None => {
                        all_present = false;
                        break;
                    }
                }
            }
            if !all_present {
                continue;
            }
            // Build `SELECT COUNT(*) FROM <table> WHERE c1 = ? AND c2 = ?
            // [...] [AND pk <> ?]` with dialect-aware quoting + placeholders.
            //
            // `COUNT(*)` (not `SELECT 1 … LIMIT 1`) because the result is
            // decoded as `(i64,)`: PG types the literal `1` as `INT4`, so
            // `SELECT 1` fails to decode into `i64` on Postgres (the
            // SQLite-only test never caught it). `COUNT(*)` is `bigint` on
            // PG / MySQL / SQLite alike, so `(i64,)` decodes everywhere.
            let table_q = dialect.quote_ident(T::SCHEMA.table);
            let mut sql = format!("SELECT COUNT(*) FROM {table_q} WHERE ");
            let mut binds: Vec<crate::core::SqlValue> = Vec::new();
            let mut sep = "";
            for (i, (col, val)) in bound.iter().enumerate() {
                sql.push_str(sep);
                sep = " AND ";
                let col_q = dialect.quote_ident(col);
                let ph = dialect.placeholder(i + 1);
                sql.push_str(&format!("{col_q} = {ph}"));
                binds.push(val.clone());
            }
            let extra_pk_idx = bound.len() + 1;
            if let (Some(pk_field), Some(pk_v)) = (pk_field, pk_value) {
                let pk_col = dialect.quote_ident(pk_field.column);
                let ph = dialect.placeholder(extra_pk_idx);
                sql.push_str(&format!(" AND {pk_col} <> {ph}"));
                binds.push(pk_v.clone());
            }
            // #561 — was a 3-arm `match pool` each doing the same
            // bind-loop via per-backend `bind_sql_value_inline*` helpers
            // then `fetch_optional`. The executor's `raw_query_pool` plus
            // the canonical `bind_match!` macros already handle every
            // backend's bind shape — collapse to one call. `SELECT COUNT(*)`
            // returns exactly one row whose `(i64,)` count answers the
            // existence check (`> 0`).
            let exists = crate::sql::raw_query_pool::<(i64,)>(&sql, binds, pool)
                .await
                .map(|rows| rows.first().is_some_and(|(n,)| *n > 0))
                .map_err(|e| match e {
                    crate::sql::ExecError::Driver(err) => err,
                    other => crate::sql::sqlx::Error::Protocol(format!("{other}")),
                });
            match exists {
                Ok(true) => {
                    let label = idx.columns.join(", ");
                    let msg = format!("a row with the same ({label}) already exists");
                    for col in idx.columns {
                        errors.add(col.to_owned(), msg.clone());
                    }
                }
                Ok(false) => {}
                Err(e) => errors.add_non_field(format!("unique-together pre-check failed: {e}")),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    // (continued)

    /// Convert into an [`crate::core::InsertQuery`] ready to feed
    /// `sql::insert(&pool, &query)` or `sql::insert_pool(&pool, &query)`.
    ///
    /// Sets `returning = vec![]` and `on_conflict = None` — for
    /// upsert / RETURNING flows, build the InsertQuery yourself
    /// from `columns()` + `values()`.
    #[must_use]
    pub fn into_insert_query(self) -> crate::core::InsertQuery {
        crate::core::InsertQuery {
            model: T::SCHEMA,
            columns: self.columns,
            values: self.values,
            returning: Vec::new(),
            on_conflict: None,
        }
    }

    /// Convert into an [`crate::core::UpdateQuery`] keyed on `pk_value`.
    /// Each parsed field becomes one `Assignment`; the WHERE clause
    /// filters the model's PK column equal to `pk_value`.
    ///
    /// Returns `None` when the model has no primary key (rare; the
    /// macro layer normally requires one).
    #[must_use]
    pub fn into_update_query(
        self,
        pk_value: crate::core::SqlValue,
    ) -> Option<crate::core::UpdateQuery> {
        let pk = T::SCHEMA.primary_key()?;
        let assignments: Vec<crate::core::Assignment> = self
            .columns
            .into_iter()
            .zip(self.values)
            .map(|(column, value)| crate::core::Assignment {
                column,
                value: value.into(),
            })
            .collect();
        Some(crate::core::UpdateQuery {
            model: T::SCHEMA,
            set: assignments,
            where_clause: crate::core::WhereExpr::Predicate(crate::core::Filter {
                column: pk.column,
                op: crate::core::Op::Eq,
                value: pk_value,
            }),
        })
    }
}

#[cfg(test)]
mod model_form_tests {
    use super::*;
    use crate::sql::Auto;

    #[derive(crate::Model, Debug)]
    #[rustango(table = "mf_post")]
    #[allow(dead_code)]
    pub struct Post {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 50)]
        pub title: String,
        pub body: Option<String>,
    }

    #[test]
    fn parse_skips_auto_pk_and_accepts_required_string() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "hi".into());
        p.insert("body".into(), "".into());
        let mf: ModelFormFor<Post> = ModelFormFor::<Post>::parse(&p).expect("valid");
        // Auto<i64> PK skipped → title + body present.
        assert_eq!(mf.columns(), &["title", "body"]);
    }

    #[test]
    fn parse_collects_multiple_errors() {
        let mut p: HashMap<String, String> = HashMap::new();
        // title missing entirely + > max_length when present
        p.insert("title".into(), "x".repeat(100));
        let err = ModelFormFor::<Post>::parse(&p).expect_err("should error");
        // At least the title bound violation surfaces.
        assert!(!err.is_empty(), "errors should be non-empty");
    }

    #[test]
    fn into_insert_query_targets_correct_model() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "hi".into());
        let mf: ModelFormFor<Post> = ModelFormFor::<Post>::parse(&p).expect("valid");
        let q = mf.into_insert_query();
        assert_eq!(q.model.table, "mf_post");
        assert!(q.columns.contains(&"title"));
    }

    #[test]
    fn from_json_parses_object_body() {
        let v = serde_json::json!({ "title": "from-json", "body": "x" });
        let mf: ModelFormFor<Post> = ModelFormFor::<Post>::from_json(&v).expect("valid");
        assert!(mf.columns().contains(&"title"));
        assert!(mf.columns().contains(&"body"));
    }

    #[test]
    fn from_json_handles_numeric_values() {
        // i64-typed fields accept both `42` and `"42"`. Use the
        // existing Post struct: title is string-typed so cover the
        // numeric path with a string→number-stringified round trip.
        let v = serde_json::json!({ "title": 42 });
        // 42 → "42" → SqlValue::String("42") → fits a String field.
        let mf: ModelFormFor<Post> = ModelFormFor::<Post>::from_json(&v).expect("valid");
        match mf.values().iter().find(|_| true).unwrap() {
            crate::core::SqlValue::String(s) => assert_eq!(s, "42"),
            other => panic!("expected stringified value, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_non_object_root() {
        let v = serde_json::json!(["title", "body"]);
        let err = ModelFormFor::<Post>::from_json(&v).expect_err("array body should error");
        assert!(!err.non_field().is_empty());
    }

    #[test]
    fn from_json_treats_null_as_absent() {
        // body is Option<String> — JSON null should land as
        // SqlValue::Null in the assignments (parse_form_value treats
        // missing-string for nullable as Null).
        let v = serde_json::json!({ "title": "ok", "body": null });
        let mf: ModelFormFor<Post> = ModelFormFor::<Post>::from_json(&v).expect("valid");
        let body_value = mf
            .columns()
            .iter()
            .position(|c| *c == "body")
            .and_then(|i| mf.values().get(i));
        assert!(matches!(body_value, Some(crate::core::SqlValue::Null)));
    }

    #[test]
    fn into_update_query_filters_on_pk() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "edited".into());
        // `body` is `Option<String>` and absent → parsed as
        // `SqlValue::Null` (valid for a nullable column) and
        // included in the assignment list. Two assignments expected:
        // title (edited) + body (NULL).
        let mf: ModelFormFor<Post> = ModelFormFor::<Post>::parse(&p).expect("valid");
        let q = mf
            .into_update_query(crate::core::SqlValue::I64(42))
            .expect("model has PK");
        assert_eq!(q.set.len(), 2);
        match &q.where_clause {
            crate::core::WhereExpr::Predicate(f) => {
                assert_eq!(f.column, "id");
                assert_eq!(f.value, crate::core::SqlValue::I64(42));
            }
            _ => panic!("wrong where shape"),
        }
    }

    // ---- v0.49 — ModelForm field-include / field-exclude semantics ----

    /// `<Post as crate::core::Model>::SCHEMA` shorthand for the
    /// builder tests below.
    fn post_schema() -> &'static crate::core::ModelSchema {
        <Post as crate::core::Model>::SCHEMA
    }

    #[test]
    fn modelform_default_includes_all_non_pk_non_auto_fields() {
        let form = ModelForm::new(post_schema(), HashMap::new());
        // Post: id is auto-PK (excluded), title + body remain.
        let included = form.included_field_names();
        assert_eq!(included, vec!["title", "body"]);
    }

    #[test]
    fn modelform_fields_restricts_to_named_set() {
        let form = ModelForm::new(post_schema(), HashMap::new()).fields(&["title"]);
        assert_eq!(form.included_field_names(), vec!["title"]);
    }

    #[test]
    fn modelform_exclude_drops_named_fields() {
        let form = ModelForm::new(post_schema(), HashMap::new()).exclude(&["body"]);
        assert_eq!(form.included_field_names(), vec!["title"]);
    }

    #[test]
    fn modelform_exclude_and_fields_compose() {
        // `.fields()` whitelists, then `.exclude()` removes —
        // Django's `Meta.fields` + `Meta.exclude` interaction.
        let form = ModelForm::new(post_schema(), HashMap::new())
            .fields(&["title", "body"])
            .exclude(&["body"]);
        assert_eq!(form.included_field_names(), vec!["title"]);
    }

    #[test]
    fn modelform_exclude_cannot_re_enable_pk_or_auto() {
        // Auto / PK fields are always excluded regardless of the
        // exclude list (excluding `id` doesn't change anything).
        let form = ModelForm::new(post_schema(), HashMap::new()).exclude(&["id"]);
        // title + body still present (id was already excluded).
        assert_eq!(form.included_field_names(), vec!["title", "body"]);
    }

    #[test]
    fn modelform_exclude_unknown_field_is_a_no_op() {
        let form = ModelForm::new(post_schema(), HashMap::new()).exclude(&["nope"]);
        assert_eq!(form.included_field_names(), vec!["title", "body"]);
    }

    // ---- #375 — prepare_save / PreparedSave (commit=False) ----

    #[test]
    fn prepare_save_returns_validation_error_when_form_invalid() {
        // title is required (non-nullable String) — omitting it
        // surfaces as a FormError::Missing through validate().
        let p: HashMap<String, String> = HashMap::new();
        let form = ModelForm::new(post_schema(), p);
        let err = form.prepare_save().expect_err("should fail validation");
        assert!(matches!(err, ModelFormError::Validation(_)));
    }

    #[test]
    fn prepare_save_walks_only_form_supplied_fields_on_insert() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "ok".into());
        // body deliberately omitted from form — `parse_form_value`
        // treats an absent nullable as `SqlValue::Null` and includes
        // it, so the prepared write set carries both columns.
        let prep = ModelForm::new(post_schema(), p)
            .prepare_save()
            .expect("valid");
        assert!(prep.is_insert(), "no pk_value supplied → INSERT");
        assert!(prep.has("title"));
        assert!(prep.has("body"));
    }

    #[test]
    fn prepared_save_set_adds_missing_field() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "ok".into());
        let form = ModelForm::new(post_schema(), p).exclude(&["body"]);
        let mut prep = form.prepare_save().expect("valid");
        assert!(!prep.has("body"), "body was excluded by the form");
        prep.set("body", SqlValue::String("late binding".into()));
        assert!(
            prep.has("body"),
            "set() should have added body to the write set"
        );
    }

    #[test]
    fn prepared_save_set_overrides_existing_field() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "from-form".into());
        let mut prep = ModelForm::new(post_schema(), p)
            .prepare_save()
            .expect("valid");
        prep.set("title", SqlValue::String("override".into()));
        // The override should fully replace, not duplicate — search
        // by FieldSchema for the column name then count occurrences.
        let title_col = post_schema()
            .scalar_fields()
            .find(|f| f.name == "title")
            .unwrap()
            .column;
        assert_eq!(
            prep.columns.iter().filter(|c| **c == title_col).count(),
            1,
            "title should appear exactly once"
        );
    }

    #[test]
    fn prepared_save_unset_drops_field_from_write_set() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "ok".into());
        let mut prep = ModelForm::new(post_schema(), p)
            .prepare_save()
            .expect("valid");
        assert!(prep.has("body"));
        prep.unset("body");
        assert!(!prep.has("body"), "unset() should drop body");
    }

    #[test]
    fn prepared_save_set_unknown_field_is_a_noop() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "ok".into());
        let mut prep = ModelForm::new(post_schema(), p)
            .prepare_save()
            .expect("valid");
        prep.set("nonexistent", SqlValue::I64(0));
        // No new column should appear in the prepared write set.
        assert!(
            prep.columns.iter().all(|c| *c != "nonexistent"),
            "unknown field name should not pollute the write set"
        );
    }

    #[test]
    fn prepare_save_carries_pk_for_update_path() {
        let mut p: HashMap<String, String> = HashMap::new();
        p.insert("title".into(), "edited".into());
        let prep = ModelForm::for_update(post_schema(), p, SqlValue::I64(42))
            .prepare_save()
            .expect("valid");
        assert!(!prep.is_insert(), "pk_value was supplied → UPDATE");
    }
}
