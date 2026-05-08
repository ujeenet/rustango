//! End-to-end check that `#[derive(Model)]` emits the expected schema and
//! that the model is discoverable via the inventory registry.

use rustango::core::{inventory, FieldType, Model as _, ModelEntry};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "auth_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(column = "user_name")]
    name: String,
    email: Option<String>,
    is_active: bool,
}

#[derive(Model)]
#[allow(dead_code)]
pub struct BlogPost {
    #[rustango(primary_key)]
    id: i64,
    title: String,
}

#[derive(Model)]
#[rustango(display = "title")]
#[allow(dead_code)]
pub struct DisplayedPost {
    #[rustango(primary_key)]
    id: i64,
    title: String,
}

#[derive(Model)]
#[rustango(table = "default_demo")]
#[allow(dead_code)]
pub struct DefaultDemo {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(default = "0")]
    score: i32,
    #[rustango(max_length = 16, default = "'draft'")]
    status: String,
    #[rustango(default = "true")]
    is_active: bool,
    name: String,
}

#[test]
fn schema_reflects_attributes() {
    let s = User::SCHEMA;
    assert_eq!(s.name, "User");
    assert_eq!(s.table, "auth_user");
    assert_eq!(s.fields.len(), 4);

    let id = s.field("id").unwrap();
    assert_eq!(id.column, "id");
    assert_eq!(id.ty, FieldType::I64);
    assert!(id.primary_key);
    assert!(!id.nullable);

    let name = s.field("name").unwrap();
    assert_eq!(name.column, "user_name");
    assert_eq!(name.ty, FieldType::String);
    assert!(!name.nullable);

    let email = s.field("email").unwrap();
    assert_eq!(email.ty, FieldType::String);
    assert!(email.nullable);

    assert_eq!(s.primary_key().map(|f| f.name), Some("id"));
}

#[test]
fn default_table_is_snake_case() {
    assert_eq!(BlogPost::SCHEMA.table, "blog_post");
}

#[test]
fn display_attribute_lands_in_schema() {
    assert_eq!(DisplayedPost::SCHEMA.display, Some("title"));
    let f = DisplayedPost::SCHEMA.display_field().unwrap();
    assert_eq!(f.name, "title");
}

#[test]
fn display_defaults_to_primary_key_when_unset() {
    assert_eq!(BlogPost::SCHEMA.display, None);
    let f = BlogPost::SCHEMA.display_field().unwrap();
    assert_eq!(f.name, "id");
    assert!(f.primary_key);
}

#[test]
fn default_attribute_lands_in_schema() {
    let s = DefaultDemo::SCHEMA;
    assert_eq!(s.field("score").unwrap().default, Some("0"));
    assert_eq!(s.field("status").unwrap().default, Some("'draft'"));
    assert_eq!(s.field("is_active").unwrap().default, Some("true"));
}

#[test]
fn default_unset_remains_none() {
    assert_eq!(DefaultDemo::SCHEMA.field("name").unwrap().default, None);
    // Sanity: unrelated models are untouched.
    assert_eq!(BlogPost::SCHEMA.field("title").unwrap().default, None);
}

#[test]
fn models_register_with_inventory() {
    let registered: Vec<&'static str> = inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema.name)
        .collect();
    assert!(
        registered.contains(&"User"),
        "User missing from registry: {registered:?}"
    );
    assert!(
        registered.contains(&"BlogPost"),
        "BlogPost missing from registry: {registered:?}",
    );
}

// ---------- ForeignKey<T, K> non-i64 PK shapes ----------

#[derive(Model, Clone)]
#[rustango(table = "fk_uuid_parent")]
#[allow(dead_code)]
pub struct UuidParent {
    #[rustango(primary_key)]
    id: uuid::Uuid,
    name: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_uuid_child")]
#[allow(dead_code)]
pub struct UuidChild {
    #[rustango(primary_key)]
    id: i64,
    parent: rustango::sql::ForeignKey<UuidParent, uuid::Uuid>,
    label: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_string_parent")]
#[allow(dead_code)]
pub struct StringPkParent {
    #[rustango(primary_key, max_length = 36)]
    user_uuid: String,
    name: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_string_child")]
#[allow(dead_code)]
pub struct StringPkChild {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 36, on = "user_uuid")]
    parent: rustango::sql::ForeignKey<StringPkParent, String>,
    body: String,
}

// Plain `ForeignKey<Parent>` (no K) — the v0.7 BIGINT shape stays
// the default. Backward-compat regression guard.
#[derive(Model, Clone)]
#[rustango(table = "fk_i64_parent")]
#[allow(dead_code)]
pub struct I64Parent {
    #[rustango(primary_key)]
    id: i64,
    name: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_i64_child")]
#[allow(dead_code)]
pub struct I64Child {
    #[rustango(primary_key)]
    id: i64,
    parent: rustango::sql::ForeignKey<I64Parent>,
    label: String,
}

#[test]
fn foreign_key_uuid_emits_uuid_column_type() {
    let s = UuidChild::SCHEMA;
    let f = s.field("parent").expect("parent column on UuidChild");
    assert_eq!(f.ty, FieldType::Uuid);
    assert!(
        f.relation.is_some(),
        "ForeignKey<UuidParent, Uuid> should still set Relation::Fk"
    );
}

#[test]
fn foreign_key_string_emits_string_column_type() {
    let s = StringPkChild::SCHEMA;
    let f = s.field("parent").expect("parent column on StringPkChild");
    assert_eq!(f.ty, FieldType::String);
    assert!(f.relation.is_some());
}

#[test]
fn foreign_key_default_i64_unchanged() {
    let f = I64Child::SCHEMA.field("parent").unwrap();
    assert_eq!(f.ty, FieldType::I64);
    assert!(f.relation.is_some());
}

// ---- Nullable FK shapes ----

#[derive(Model, Clone)]
#[rustango(table = "fk_nullable_parent")]
#[allow(dead_code)]
pub struct NullableParent {
    #[rustango(primary_key)]
    id: i64,
    name: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_nullable_child")]
#[allow(dead_code)]
pub struct NullableChild {
    #[rustango(primary_key)]
    id: i64,
    /// Nullable i64 FK — should compile and emit a nullable column.
    parent: Option<rustango::sql::ForeignKey<NullableParent>>,
    label: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_nullable_str_parent")]
#[allow(dead_code)]
pub struct NullableStrParent {
    #[rustango(primary_key, max_length = 36)]
    user_uuid: String,
    name: String,
}

#[derive(Model, Clone)]
#[rustango(table = "fk_nullable_str_child")]
#[allow(dead_code)]
pub struct NullableStrChild {
    #[rustango(primary_key)]
    id: i64,
    /// Nullable String FK — covers the cross-product of nullable
    /// + non-i64 PK.
    #[rustango(max_length = 36, on = "user_uuid")]
    parent: Option<rustango::sql::ForeignKey<NullableStrParent, String>>,
    label: String,
}

#[test]
fn option_foreign_key_compiles_and_marks_nullable() {
    let f = NullableChild::SCHEMA.field("parent").unwrap();
    assert_eq!(f.ty, FieldType::I64);
    assert!(f.nullable, "Option<ForeignKey<…>> should be nullable");
    assert!(f.relation.is_some());
}

#[test]
fn option_foreign_key_string_pk_compiles() {
    let f = NullableStrChild::SCHEMA.field("parent").unwrap();
    assert_eq!(f.ty, FieldType::String);
    assert!(f.nullable);
    assert!(f.relation.is_some());
}

// ---------- i16 (SMALLINT) ----------

#[derive(Model, Clone)]
#[rustango(table = "i16_status")]
#[allow(dead_code)]
pub struct I16Status {
    #[rustango(primary_key)]
    id: i64,
    /// Bounded status code — fits in i16, saves bytes vs i64.
    code: i16,
    /// Optional priority — covers nullable i16.
    priority: Option<i16>,
}

#[test]
fn i16_field_emits_i16_field_type() {
    let s = I16Status::SCHEMA;
    let code = s.field("code").unwrap();
    assert_eq!(code.ty, FieldType::I16);
    assert!(!code.nullable);

    let priority = s.field("priority").unwrap();
    assert_eq!(priority.ty, FieldType::I16);
    assert!(priority.nullable);
}

// ----- v0.27.2 (#62, #67) — admin visibility regression guard -----
//
// Pre-0.27.2: `permissions: bool` defaulted to `false`. Models without
// an explicit `#[rustango(permissions)]` attribute were skipped by
// `auto_create_permissions`, never had `{table}.view` codenames seeded
// in the catalog, and were therefore invisible to non-superuser tenant
// admins. The startapp scaffolder produced models without the flag, so
// fresh apps appeared to be broken (#62).
//
// Post-0.27.2: default flipped to `true`; opt out with
// `#[rustango(permissions = false)]`. These tests assert the default
// across the three relevant cases.

#[derive(Model, Debug)]
#[rustango(table = "perm_default")]
#[allow(dead_code)]
pub struct PermDefault {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug)]
#[rustango(table = "perm_explicit_true", permissions)]
#[allow(dead_code)]
pub struct PermExplicitTrue {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug)]
#[rustango(table = "perm_explicit_false", permissions = false)]
#[allow(dead_code)]
pub struct PermExplicitFalse {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn permissions_defaults_to_true() {
    // Without `#[rustango(permissions)]` the model still gets the
    // catalog seeded — the historical regression that hid scaffolded
    // models from the tenant admin sidebar.
    assert!(
        PermDefault::SCHEMA.permissions,
        "regression: default `permissions` should be `true` so non-superusers see scaffolded models"
    );
}

#[test]
fn permissions_explicit_true_round_trips() {
    assert!(PermExplicitTrue::SCHEMA.permissions);
}

#[test]
fn permissions_explicit_false_opts_out() {
    // Registry-internal models that don't want to appear in tenant
    // permissions catalog opt out.
    assert!(!PermExplicitFalse::SCHEMA.permissions);
}
