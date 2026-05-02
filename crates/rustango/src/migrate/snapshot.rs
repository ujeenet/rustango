//! Schema snapshots — serializable mirror of the inventory registry.
//!
//! v0.2 captures table + column metadata in JSON so two snapshots can be
//! diffed to produce DDL. Only the fields the writer cares about are
//! tracked: type, nullability, primary key, `max_length`, min/max,
//! relations. Per-field bounds become `CHECK` constraints; relations
//! become `FOREIGN KEY` ALTER statements.

use crate::core::{inventory, FieldType, ModelEntry, ModelSchema, Relation};
use serde::{Deserialize, Serialize};

/// A snapshot of every registered model, ordered by table name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SchemaSnapshot {
    pub tables: Vec<TableSnapshot>,
    /// Junction tables derived from `ModelSchema::m2m` declarations,
    /// sorted by `through` name. Absent from old migration files — the
    /// `#[serde(default)]` produces an empty vec, which is correct
    /// (no M2M tables in older snapshots).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub m2m_tables: Vec<M2MTableSnapshot>,
    /// Indexes derived from `ModelSchema::indexes` declarations, sorted
    /// by name. Absent from old migration files — defaults to empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<IndexSnapshot>,
}

/// Snapshot of one `CREATE INDEX` declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexSnapshot {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// Snapshot of one many-to-many junction table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct M2MTableSnapshot {
    /// SQL name of the junction table (e.g. `"post_tags"`).
    pub through: String,
    /// SQL name of the source model's table (e.g. `"posts"`).
    pub src_table: String,
    /// FK column in the junction table pointing to the source (e.g. `"post_id"`).
    pub src_col: String,
    /// SQL name of the target model's table (e.g. `"app_tags"`).
    pub dst_table: String,
    /// FK column in the junction table pointing to the target (e.g. `"tag_id"`).
    pub dst_col: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableSnapshot {
    pub name: String,
    pub model: String,
    pub fields: Vec<FieldSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldSnapshot {
    pub name: String,
    pub column: String,
    pub ty: String,
    pub nullable: bool,
    pub primary_key: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max: Option<i64>,
    /// Raw SQL fragment for `DEFAULT` if the model declared one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub default: Option<String>,
    /// `true` for fields whose Rust type is `Auto<T>` — server-assigned
    /// PKs that translate to `BIGSERIAL` / `SERIAL` in DDL. Skipped on
    /// serialize when `false` so older snapshots stay diff-clean.
    #[serde(skip_serializing_if = "is_false", default)]
    pub auto: bool,
    /// `true` when `#[rustango(unique)]` was declared. Skipped on
    /// serialize when `false` to keep snapshots diff-clean.
    #[serde(skip_serializing_if = "is_false", default)]
    pub unique: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fk: Option<RelationSnapshot>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationSnapshot {
    /// `"fk"` or `"o2o"`.
    pub kind: String,
    pub to: String,
    pub on: String,
}

impl SchemaSnapshot {
    /// Capture every model registered in the binary's `inventory`.
    #[must_use]
    pub fn from_registry() -> Self {
        let entries: Vec<&ModelEntry> = inventory::iter::<ModelEntry>.into_iter().collect();
        let mut tables: Vec<TableSnapshot> =
            entries.iter().map(|e| TableSnapshot::from_schema(e.schema)).collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(entries.iter().map(|e| e.schema));
        let indexes = collect_indexes(entries.iter().map(|e| e.schema));
        Self { tables, m2m_tables, indexes }
    }

    /// Capture only the models whose [`ModelEntry::resolved_app_label`]
    /// matches `app`. Powers `manage makemigrations <app>` — diffs
    /// just one app's models against the latest snapshot and emits a
    /// migration scoped to that app.
    ///
    /// Models with no app label (project-root models) are excluded —
    /// they belong to the project's flat `migrations/` dir, not to a
    /// sub-app's `migrations/<app>/`.
    #[must_use]
    pub fn from_registry_for_app(app: &str) -> Self {
        let entries: Vec<&ModelEntry> = inventory::iter::<ModelEntry>
            .into_iter()
            .filter(|e| e.resolved_app_label() == Some(app))
            .collect();
        let mut tables: Vec<TableSnapshot> =
            entries.iter().map(|e| TableSnapshot::from_schema(e.schema)).collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(entries.iter().map(|e| e.schema));
        let indexes = collect_indexes(entries.iter().map(|e| e.schema));
        Self { tables, m2m_tables, indexes }
    }

    /// Capture an explicit list of model schemas — the inventory-
    /// agnostic counterpart of [`from_registry`]. Used by callers that
    /// want a curated snapshot rather than every linked model (e.g.
    /// `rustango-tenancy`'s bootstrap migrations, which pin themselves
    /// to `rustango_orgs` + `rustango_operators` + `rustango_users`).
    #[must_use]
    pub fn from_models(models: &[&ModelSchema]) -> Self {
        let mut tables: Vec<TableSnapshot> =
            models.iter().map(|s| TableSnapshot::from_schema(s)).collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(models.iter().copied());
        let indexes = collect_indexes(models.iter().copied());
        Self { tables, m2m_tables, indexes }
    }

    /// Look up an M2M table snapshot by junction table name.
    #[must_use]
    pub fn m2m_table(&self, through: &str) -> Option<&M2MTableSnapshot> {
        self.m2m_tables.iter().find(|t| t.through == through)
    }

    /// Look up an index snapshot by name.
    #[must_use]
    pub fn index(&self, name: &str) -> Option<&IndexSnapshot> {
        self.indexes.iter().find(|i| i.name == name)
    }

    /// Look up a table by SQL name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableSnapshot> {
        self.tables.iter().find(|t| t.name == name)
    }
}

impl TableSnapshot {
    /// Build a snapshot row from a registered [`ModelSchema`]. Public
    /// so external callers (e.g. tenancy bootstrap migrations) can
    /// assemble their own snapshots without going through the global
    /// inventory.
    #[must_use]
    pub fn from_schema(s: &ModelSchema) -> Self {
        let mut fields: Vec<FieldSnapshot> =
            s.scalar_fields().map(FieldSnapshot::from_schema).collect();
        fields.sort_by(|a, b| a.column.cmp(&b.column));
        Self {
            name: s.table.to_owned(),
            model: s.name.to_owned(),
            fields,
        }
    }

    /// Look up a field by SQL column name.
    #[must_use]
    pub fn field(&self, column: &str) -> Option<&FieldSnapshot> {
        self.fields.iter().find(|f| f.column == column)
    }
}

impl FieldSnapshot {
    fn from_schema(f: &crate::core::FieldSchema) -> Self {
        let fk = f.relation.and_then(|r| match r {
            Relation::Fk { to, on } => Some(RelationSnapshot {
                kind: "fk".into(),
                to: to.to_owned(),
                on: on.to_owned(),
            }),
            Relation::O2O { to, on } => Some(RelationSnapshot {
                kind: "o2o".into(),
                to: to.to_owned(),
                on: on.to_owned(),
            }),
        });
        Self {
            name: f.name.to_owned(),
            column: f.column.to_owned(),
            ty: field_type_name(f.ty).to_owned(),
            nullable: f.nullable,
            primary_key: f.primary_key,
            max_length: f.max_length,
            min: f.min,
            max: f.max,
            default: f.default.map(str::to_owned),
            auto: f.auto,
            unique: f.unique,
            fk,
        }
    }
}

fn field_type_name(ty: FieldType) -> &'static str {
    // Reuse the `FieldType::as_str` mapping but with stable JSON names.
    match ty {
        FieldType::I32 => "i32",
        FieldType::I64 => "i64",
        FieldType::F32 => "f32",
        FieldType::F64 => "f64",
        FieldType::Bool => "bool",
        FieldType::String => "string",
        FieldType::DateTime => "datetime",
        FieldType::Date => "date",
        FieldType::Uuid => "uuid",
        FieldType::Json => "json",
    }
}

/// Collect all `CREATE INDEX` descriptors from a set of model schemas,
/// deduplicating by index name and sorting for deterministic output.
fn collect_indexes<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<IndexSnapshot> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<IndexSnapshot> = Vec::new();
    for schema in schemas {
        for idx in schema.indexes {
            if seen.insert(idx.name) {
                out.push(IndexSnapshot {
                    name: idx.name.to_owned(),
                    table: schema.table.to_owned(),
                    columns: idx.columns.iter().map(|&c| c.to_owned()).collect(),
                    unique: idx.unique,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Collect all M2M junction table descriptors from a set of model schemas,
/// deduplicating by `through` table name and sorting for deterministic output.
fn collect_m2m_tables<'a>(
    schemas: impl Iterator<Item = &'a ModelSchema>,
) -> Vec<M2MTableSnapshot> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<M2MTableSnapshot> = Vec::new();
    for schema in schemas {
        for rel in schema.m2m {
            if seen.insert(rel.through) {
                out.push(M2MTableSnapshot {
                    through: rel.through.to_owned(),
                    src_table: schema.table.to_owned(),
                    src_col: rel.src_col.to_owned(),
                    dst_table: rel.to.to_owned(),
                    dst_col: rel.dst_col.to_owned(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.through.cmp(&b.through));
    out
}
