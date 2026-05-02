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

use crate::core::{Assignment, FieldSchema, FieldType, Filter, ModelSchema, Op, SqlValue, UpdateQuery, WhereExpr, InsertQuery};

#[cfg(feature = "csrf")]
pub mod csrf;

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
        self.fields.entry(field.into()).or_default().push(msg.into());
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

/// Backwards-compatible alias — prefer [`Form`].
#[deprecated(since = "0.16.0", note = "use `Form` and `FormErrors` instead")]
pub trait FormStruct: Sized {
    fn parse(form: &HashMap<String, String>) -> Result<Self, FormError>;
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
        | FieldType::Json => Err(FormError::UnsupportedPk {
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
                serde_json::from_str::<serde_json::Value>(raw).map(SqlValue::Json).map_err(|e| {
                    FormError::Parse {
                        field: field.name.to_owned(),
                        ty: "Json",
                        value: raw.to_owned(),
                        detail: e.to_string(),
                    }
                })
            }
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
        if skip.contains(&field.name) {
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
}

impl ModelForm {
    /// Create a form for **inserting** a new row.
    pub fn new(schema: &'static ModelSchema, data: HashMap<String, String>) -> Self {
        Self { schema, data, pk_value: None, include_fields: None }
    }

    /// Create a form for **updating** the row identified by `pk`.
    pub fn for_update(
        schema: &'static ModelSchema,
        data: HashMap<String, String>,
        pk: SqlValue,
    ) -> Self {
        Self { schema, data, pk_value: Some(pk), include_fields: None }
    }

    /// Restrict the form to only the named fields. By default all
    /// non-PK, non-auto scalar fields are included.
    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.include_fields = Some(fields.iter().map(|&s| s.to_owned()).collect());
        self
    }

    fn should_include(&self, field: &FieldSchema) -> bool {
        if field.primary_key || field.auto {
            return false;
        }
        match &self.include_fields {
            Some(list) => list.iter().any(|n| n == field.name),
            None => true,
        }
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
    /// # Errors
    /// [`ModelFormError::Validation`] if any field is invalid.
    /// [`ModelFormError::Database`] for driver-level failures.
    pub async fn save(
        &self,
        pool: &crate::sql::sqlx::PgPool,
    ) -> Result<SqlValue, ModelFormError> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(ModelFormError::Validation(errors));
        }

        let pk_field = self.schema.primary_key().ok_or_else(|| {
            ModelFormError::Database(crate::sql::ExecError::Driver(
                sqlx::Error::Protocol("model has no primary key".into()),
            ))
        })?;

        if let Some(pk_val) = &self.pk_value {
            // UPDATE
            let assignments: Vec<Assignment> = self
                .schema
                .scalar_fields()
                .filter(|f| self.should_include(f))
                .filter_map(|f| {
                    let raw = self.data.get(f.name).map(String::as_str);
                    parse_form_value(f, raw).ok().map(|v| Assignment { column: f.column, value: v })
                })
                .collect();

            let query = UpdateQuery {
                model: self.schema,
                set: assignments,
                where_clause: WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_val.clone(),
                }),
            };
            crate::sql::update(pool, &query).await?;
            Ok(pk_val.clone())
        } else {
            // INSERT
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
            let query = InsertQuery {
                model: self.schema,
                columns,
                values,
                returning: vec![pk_field.column],
            };
            let row = crate::sql::insert_returning(pool, &query).await?;
            use crate::sql::sqlx::Row as _;
            let pk_val: SqlValue = match pk_field.ty {
                FieldType::I64 => SqlValue::I64(row.try_get(pk_field.column).unwrap_or(0)),
                FieldType::I32 => SqlValue::I32(row.try_get(pk_field.column).unwrap_or(0)),
                FieldType::String => {
                    SqlValue::String(row.try_get(pk_field.column).unwrap_or_default())
                }
                _ => SqlValue::Null,
            };
            Ok(pk_val)
        }
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

fn bool_true() -> bool { true }

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
                None | Some("") if field.required && field.field_type != DynamicFieldType::Boolean => {
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
                                        errors.add(&field.name, format!("Ensure this value is ≥ {min}."));
                                    }
                                }
                                if let Some(max) = field.max {
                                    if (n as f64) > max {
                                        errors.add(&field.name, format!("Ensure this value is ≤ {max}."));
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
                                        errors.add(&field.name, format!("Ensure this value is ≥ {min}."));
                                    }
                                }
                                if let Some(max) = field.max {
                                    if n > max {
                                        errors.add(&field.name, format!("Ensure this value is ≤ {max}."));
                                    }
                                }
                            }
                            Err(_) => errors.add(&field.name, "Enter a number."),
                        }
                    }
                }
                DynamicFieldType::Text | DynamicFieldType::Textarea | DynamicFieldType::Email | DynamicFieldType::Url => {
                    if let Some(max) = field.max_length {
                        if raw_str.len() > max {
                            errors.add(&field.name, format!("Ensure this value has at most {max} characters."));
                        }
                    }
                    if let Some(min) = field.min_length {
                        if !raw_str.is_empty() && raw_str.len() < min {
                            errors.add(&field.name, format!("Ensure this value has at least {min} characters."));
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
                            || chrono::NaiveDateTime::parse_from_str(raw_str, "%Y-%m-%dT%H:%M:%S").is_ok()
                            || chrono::NaiveDateTime::parse_from_str(raw_str, "%Y-%m-%dT%H:%M").is_ok();
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
                DynamicFieldType::Boolean => serde_json::Value::Bool(
                    !matches!(raw.to_ascii_lowercase().as_str(), "" | "false" | "0" | "off" | "no"),
                ),
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
