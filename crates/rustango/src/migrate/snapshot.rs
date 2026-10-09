//! Schema snapshots: a serializable copy of the model registry.
//!
//! A snapshot records table and column metadata as JSON, so two of
//! them can be diffed into DDL. It keeps only what the DDL writer
//! needs: type, nullability, primary key, `max_length`, min/max and
//! relations. Bounds become `CHECK` constraints and relations become
//! `FOREIGN KEY` statements.

use crate::core::{inventory, FieldType, ModelEntry, ModelSchema, Relation};
use serde::{Deserialize, Serialize};

/// Framework tables that exist in **every** rustango database, the
/// registry and every tenant, because both need audit rows and a
/// content-type catalog. A migration has one scope and cannot say
/// "both", so these appear in every scope's system migrations and
/// each database gets its own copy.
pub const SHARED_SYSTEM_TABLES: &[&str] = &["rustango_audit_log", "rustango_content_types"];

/// `true` when at least one framework model *declares* `scope`.
///
/// This is **not** the same as "the scope's snapshot is non-empty".
/// [`SHARED_SYSTEM_TABLES`] belong to the tenant scope but are copied
/// into every scope, so a registry snapshot is never empty even in a
/// build with no registry database.
#[must_use]
pub fn scope_owns_system_tables(scope: crate::core::ModelScope) -> bool {
    inventory::iter::<ModelEntry>
        .into_iter()
        .any(|e| e.schema.scope == scope && in_system_chain(e.schema, scope))
}

/// Whether `scope`'s system migrations create `schema`'s table.
#[must_use]
pub(crate) fn in_system_chain(schema: &ModelSchema, scope: crate::core::ModelScope) -> bool {
    (schema.scope == scope || SHARED_SYSTEM_TABLES.contains(&schema.table))
        && schema.table.starts_with("rustango_")
        && !schema.is_view
        && schema.managed
}

/// A snapshot of every registered model, ordered by table name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SchemaSnapshot {
    pub tables: Vec<TableSnapshot>,
    /// Junction tables from `ModelSchema::m2m`, sorted by `through`.
    /// Older snapshot files have no such key and load as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub m2m_tables: Vec<M2MTableSnapshot>,
    /// Indexes from `ModelSchema::indexes`, sorted by name. Older
    /// snapshot files load as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<IndexSnapshot>,
    /// CHECK constraints from `ModelSchema::check_constraints`, sorted
    /// by name. Older snapshot files load as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckSnapshot>,
    /// Postgres `EXCLUDE` constraints from
    /// `ModelSchema::exclusion_constraints`, sorted by name. Postgres
    /// only: MySQL and SQLite render nothing and log a warning. Older
    /// snapshot files load as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<ExclusionSnapshot>,
}

/// Snapshot of one Postgres `EXCLUDE` constraint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, PartialOrd)]
pub struct ExclusionSnapshot {
    pub name: String,
    pub table: String,
    pub using: String,
    /// `(column, operator)` pairs in declaration order.
    pub elements: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub where_clause: Option<String>,
}

/// Snapshot of one table-level CHECK constraint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, PartialOrd)]
pub struct CheckSnapshot {
    pub name: String,
    pub table: String,
    pub expr: String,
}

/// Snapshot of one `CREATE INDEX` declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, PartialOrd)]
pub struct IndexSnapshot {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    /// Access method, lowercase: `btree`, `gin`, `gist`, `brin`,
    /// `spgist`, `hash` or `bloom`. Missing or unknown means `btree`,
    /// so older snapshot files still load.
    #[serde(default = "default_index_method")]
    pub method: String,
    /// `WHERE <expr>` clause for a partial index. `None` gives a
    /// plain index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub where_clause: Option<String>,
    /// Covering-index columns. Postgres 11 and newer only; MySQL and
    /// SQLite drop the clause and log a warning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
}

fn default_index_method() -> String {
    "btree".to_owned()
}

/// Snapshot of one many-to-many junction table.
///
/// Equality and order ignore which end is the source: a mirrored
/// declaration is the same table, and a flip must not rebuild it (#2000).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M2MTableSnapshot {
    /// Junction table name, such as `"post_tags"`.
    pub through: String,
    /// Source model's table, such as `"posts"`.
    pub src_table: String,
    /// Junction column pointing at the source, such as `"post_id"`.
    pub src_col: String,
    /// Target model's table, such as `"app_tags"`.
    pub dst_table: String,
    /// Junction column pointing at the target, such as `"tag_id"`.
    pub dst_col: String,
}

impl M2MTableSnapshot {
    /// The `(table, column)` ends, sorted.
    fn key(&self) -> (&str, [(&str, &str); 2]) {
        let mut e = [
            (&*self.src_table, &*self.src_col),
            (&*self.dst_table, &*self.dst_col),
        ];
        e.sort_unstable();
        (&self.through, e)
    }

    /// Source is the end that sorts first, so the snapshot never flips.
    fn canonical(mut self) -> Self {
        if (&self.src_table, &self.src_col) > (&self.dst_table, &self.dst_col) {
            std::mem::swap(&mut self.src_table, &mut self.dst_table);
            std::mem::swap(&mut self.src_col, &mut self.dst_col);
        }
        self
    }
}

impl PartialEq for M2MTableSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl PartialOrd for M2MTableSnapshot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.key().cmp(&other.key()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableSnapshot {
    pub name: String,
    pub model: String,
    pub fields: Vec<FieldSnapshot>,
    /// Multi-column FKs from `#[rustango(fk_composite(...))]`. Left
    /// out of the JSON when empty, so older snapshots stay
    /// diff-clean.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub composite_fks: Vec<CompositeFkSnapshot>,
}

/// Serialized [`crate::core::CompositeFkRelation`], stored per table
/// in [`TableSnapshot`] in declaration order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeFkSnapshot {
    /// Relation name, a free-form Rust identifier.
    pub name: String,
    /// Target table name.
    pub to: String,
    /// Source columns, in declaration order.
    pub from: Vec<String>,
    /// Target columns, same length and order as `from`.
    pub on: Vec<String>,
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
    /// `true` for an `Auto<T>` field: a server-assigned PK that
    /// becomes `BIGSERIAL` or `SERIAL` in DDL.
    #[serde(skip_serializing_if = "is_false", default)]
    pub auto: bool,
    /// `true` when the model declared `#[rustango(unique)]`.
    #[serde(skip_serializing_if = "is_false", default)]
    pub unique: bool,
    /// Case-insensitive text column. The DDL writer then uses
    /// `dialect.ci_text_type(max_length)`.
    #[serde(skip_serializing_if = "is_false", default)]
    pub case_insensitive: bool,
    /// SQL expression for a `GENERATED ALWAYS AS (...) STORED`
    /// column. Must be captured here: system migrations render from
    /// the snapshot, not the live registry.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generated_as: Option<String>,
    /// Column comment from `db_comment="..."`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub db_comment: Option<String>,
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
    /// The `ON DELETE` action as its SQL token: `"CASCADE"`,
    /// `"SET NULL"` and so on.
    ///
    /// Must be captured here. System migrations render from the
    /// snapshot, so an action missing from it never reaches the
    /// database and a declared `cascade` silently becomes
    /// `NO ACTION`. Older snapshots with no key load as `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub on_delete: Option<String>,
}

impl SchemaSnapshot {
    /// Capture every model registered in the binary's `inventory`.
    ///
    /// `#[rustango(view)]` models are left out. The operator owns
    /// their SQL view, so the diff must never emit `CREATE TABLE` or
    /// `DROP TABLE` for one.
    #[must_use]
    pub fn from_registry() -> Self {
        let entries: Vec<&ModelEntry> = inventory::iter::<ModelEntry>
            .into_iter()
            .filter(|e| !e.schema.is_view && e.schema.managed)
            .collect();
        let mut tables: Vec<TableSnapshot> = entries
            .iter()
            .map(|e| TableSnapshot::from_schema(e.schema))
            .collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(entries.iter().map(|e| e.schema));
        let indexes = collect_indexes(entries.iter().map(|e| e.schema));
        let checks = collect_checks(entries.iter().map(|e| e.schema));
        let excludes = collect_excludes(entries.iter().map(|e| e.schema));
        Self {
            tables,
            m2m_tables,
            indexes,
            checks,
            excludes,
        }
    }

    /// Capture only the models whose
    /// [`crate::core::ModelSchema::scope`] matches `scope`, so
    /// `makemigrations` writes registry and tenant changes into
    /// separate files tagged with the matching
    /// [`super::MigrationScope`].
    ///
    /// **The split is required.** A registry table inside a
    /// tenant-scoped migration replays under the tenant's
    /// `search_path`, where its ALTERs hit the registry copy and
    /// fail.
    #[must_use]
    pub fn from_registry_for_scope(scope: crate::core::ModelScope) -> Self {
        let entries: Vec<&ModelEntry> = inventory::iter::<ModelEntry>
            .into_iter()
            .filter(|e| e.schema.scope == scope && !e.schema.is_view && e.schema.managed)
            .collect();
        let mut tables: Vec<TableSnapshot> = entries
            .iter()
            .map(|e| TableSnapshot::from_schema(e.schema))
            .collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(entries.iter().map(|e| e.schema));
        let indexes = collect_indexes(entries.iter().map(|e| e.schema));
        let checks = collect_checks(entries.iter().map(|e| e.schema));
        let excludes = collect_excludes(entries.iter().map(|e| e.schema));
        Self {
            tables,
            m2m_tables,
            indexes,
            checks,
            excludes,
        }
    }

    /// Capture only **framework** models, those with a `rustango_`
    /// table name, whose scope matches `scope`. This is the "system
    /// app": `makemigrations` writes it to the project's
    /// `system/migrations/` folder. User models are left out so the
    /// framework's migrations never mix with the app's.
    ///
    /// Every scope also carries [`SHARED_SYSTEM_TABLES`], so a
    /// non-empty snapshot does not mean the scope is in use. Use
    /// [`scope_owns_system_tables`] for that.
    #[must_use]
    pub fn from_registry_system_for_scope(scope: crate::core::ModelScope) -> Self {
        let matching = inventory::iter::<ModelEntry>
            .into_iter()
            .filter(|e| in_system_chain(e.schema, scope));
        // Only one model may own a table, or the migration emits two
        // `CREATE TABLE`s for it. A custom user model declares
        // `rustango_users` while the built-in `User` derive is always
        // present, so both reach the inventory. Dedupe by table and
        // let the downstream model win: naming a framework table is a
        // deliberate override.
        let mut by_table: std::collections::BTreeMap<&'static str, &ModelEntry> =
            std::collections::BTreeMap::new();
        for e in matching {
            match by_table.entry(e.schema.table) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(e);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let held = *slot.get();
                    match (held.is_framework(), e.is_framework()) {
                        // Downstream model overrides the framework's own.
                        (true, false) => {
                            tracing::warn!(
                                target: "rustango::migrate",
                                table = %e.schema.table,
                                model = %e.module_path,
                                "a project model is overriding a framework table — its \
                                 schema will be used instead of rustango's. If this was \
                                 not intended, the table name is a typo."
                            );
                            slot.insert(e);
                        }
                        // Two downstream models on one table is
                        // ambiguous. Pick by module path, not
                        // inventory order, so the migration is the
                        // same on every build, and warn about it.
                        (false, false) => {
                            tracing::warn!(
                                target: "rustango::migrate",
                                table = %e.schema.table,
                                candidates = %format!("{}, {}", held.module_path, e.module_path),
                                "two models declare the same table — using the \
                                 lexicographically first module path; remove one"
                            );
                            if e.module_path < held.module_path {
                                slot.insert(e);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let entries: Vec<&ModelEntry> = by_table.into_values().collect();
        let mut tables: Vec<TableSnapshot> = entries
            .iter()
            .map(|e| TableSnapshot::from_schema(e.schema))
            .collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(entries.iter().map(|e| e.schema));
        let indexes = collect_indexes(entries.iter().map(|e| e.schema));
        let checks = collect_checks(entries.iter().map(|e| e.schema));
        let excludes = collect_excludes(entries.iter().map(|e| e.schema));
        Self {
            tables,
            m2m_tables,
            indexes,
            checks,
            excludes,
        }
    }

    /// Keep only the tables, indexes and checks whose owning model has
    /// [`crate::core::ModelSchema::scope`] equal to `scope`.
    ///
    /// Use it on an on-disk snapshot before diffing: older snapshots
    /// hold every framework table whatever scope the migration ran in.
    ///
    /// A table no longer in the inventory counts as
    /// [`ModelScope::Tenant`](crate::core::ModelScope::Tenant), which
    /// matches `MigrationScope`'s
    /// default and never pulls a removed registry table back into a
    /// tenant migration.
    #[must_use]
    pub fn filtered_to_scope(&self, scope: crate::core::ModelScope) -> Self {
        let scope_of = |table: &str| {
            ModelEntry::for_table(table).map_or(crate::core::ModelScope::Tenant, |e| e.schema.scope)
        };
        let tables: Vec<TableSnapshot> = self
            .tables
            .iter()
            .filter(|t| scope_of(&t.name) == scope)
            .cloned()
            .collect();
        // M2M, indexes and checks hang off a parent table, so keep
        // only those whose parent survived the filter.
        let table_names: std::collections::HashSet<&str> =
            tables.iter().map(|t| t.name.as_str()).collect();
        let m2m_tables = self
            .m2m_tables
            .iter()
            .filter(|m| table_names.contains(m.src_table.as_str()))
            .cloned()
            .collect();
        let indexes = self
            .indexes
            .iter()
            .filter(|i| table_names.contains(i.table.as_str()))
            .cloned()
            .collect();
        let checks = self
            .checks
            .iter()
            .filter(|c| table_names.contains(c.table.as_str()))
            .cloned()
            .collect();
        let excludes = self
            .excludes
            .iter()
            .filter(|x| table_names.contains(x.table.as_str()))
            .cloned()
            .collect();
        Self {
            tables,
            m2m_tables,
            indexes,
            checks,
            excludes,
        }
    }

    /// Capture only the models whose [`ModelEntry::resolved_app_label`]
    /// is `app`, for `manage makemigrations <app>`.
    ///
    /// Models with no app label are left out: they belong to the
    /// project's top-level `migrations/` folder.
    #[must_use]
    pub fn from_registry_for_app(app: &str) -> Self {
        let entries: Vec<&ModelEntry> = inventory::iter::<ModelEntry>
            .into_iter()
            .filter(|e| {
                e.resolved_app_label() == Some(app) && !e.schema.is_view && e.schema.managed
            })
            .collect();
        let mut tables: Vec<TableSnapshot> = entries
            .iter()
            .map(|e| TableSnapshot::from_schema(e.schema))
            .collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(entries.iter().map(|e| e.schema));
        let indexes = collect_indexes(entries.iter().map(|e| e.schema));
        let checks = collect_checks(entries.iter().map(|e| e.schema));
        let excludes = collect_excludes(entries.iter().map(|e| e.schema));
        Self {
            tables,
            m2m_tables,
            indexes,
            checks,
            excludes,
        }
    }

    /// Capture an explicit list of model schemas instead of the whole
    /// inventory. Use it for a curated snapshot, such as a bootstrap
    /// migration pinned to a few known tables.
    ///
    /// `#[rustango(view)]` models are skipped, as in
    /// [`Self::from_registry`].
    #[must_use]
    pub fn from_models(models: &[&ModelSchema]) -> Self {
        Self::build_from(models.iter().copied().filter(|s| !s.is_view && s.managed))
    }

    /// Like [`Self::from_models`], but keeps `managed = false`
    /// models. The `ensure_*` table helpers need this: their models
    /// sit outside the migration set, yet the helper still renders
    /// their `CREATE TABLE` from [`ModelSchema`] instead of
    /// hand-written per-dialect DDL. Views are still skipped.
    #[must_use]
    pub fn from_models_forced(models: &[&ModelSchema]) -> Self {
        Self::build_from(models.iter().copied().filter(|s| !s.is_view))
    }

    /// Shared body of [`Self::from_models`] and
    /// [`Self::from_models_forced`], over an already-filtered
    /// iterator.
    fn build_from<'a>(models: impl Iterator<Item = &'a ModelSchema>) -> Self {
        let models: Vec<&ModelSchema> = models.collect();
        let mut tables: Vec<TableSnapshot> = models
            .iter()
            .map(|s| TableSnapshot::from_schema(s))
            .collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let m2m_tables = collect_m2m_tables(models.iter().copied());
        let indexes = collect_indexes(models.iter().copied());
        let checks = collect_checks(models.iter().copied());
        let excludes = collect_excludes(models.iter().copied());
        Self {
            tables,
            m2m_tables,
            indexes,
            checks,
            excludes,
        }
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

    /// Look up a check-constraint snapshot by name.
    #[must_use]
    pub fn check(&self, name: &str) -> Option<&CheckSnapshot> {
        self.checks.iter().find(|c| c.name == name)
    }

    /// Look up a table by SQL name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableSnapshot> {
        self.tables.iter().find(|t| t.name == name)
    }
}

impl TableSnapshot {
    /// Build a snapshot row from a [`ModelSchema`]. Public so an
    /// outside caller can assemble its own snapshot without the
    /// global inventory.
    #[must_use]
    pub fn from_schema(s: &ModelSchema) -> Self {
        let mut fields: Vec<FieldSnapshot> =
            s.scalar_fields().map(FieldSnapshot::from_schema).collect();
        fields.sort_by(|a, b| a.column.cmp(&b.column));
        // Keep declaration order, so a reorder in the source does not
        // show up as a snapshot diff.
        let composite_fks: Vec<CompositeFkSnapshot> = s
            .composite_relations
            .iter()
            .map(|rel| CompositeFkSnapshot {
                name: rel.name.to_owned(),
                to: rel.to.to_owned(),
                from: rel.from.iter().map(|c| (*c).to_owned()).collect(),
                on: rel.on.iter().map(|c| (*c).to_owned()).collect(),
            })
            .collect();
        Self {
            name: s.table.to_owned(),
            model: s.name.to_owned(),
            fields,
            composite_fks,
        }
    }

    /// Look up a field by SQL column name.
    #[must_use]
    pub fn field(&self, column: &str) -> Option<&FieldSnapshot> {
        self.fields.iter().find(|f| f.column == column)
    }

    /// Look up a composite FK by constraint name.
    #[must_use]
    pub fn composite_fk(&self, name: &str) -> Option<&CompositeFkSnapshot> {
        self.composite_fks.iter().find(|c| c.name == name)
    }
}

impl FieldSnapshot {
    fn from_schema(f: &crate::core::FieldSchema) -> Self {
        let on_delete = f.fk_on_delete.map(|a| a.as_sql().to_owned());
        let fk = f.relation.and_then(|r| match r {
            Relation::Fk { to, on } => Some(RelationSnapshot {
                kind: "fk".into(),
                to: to.to_owned(),
                on: on.to_owned(),
                on_delete: on_delete.clone(),
            }),
            Relation::O2O { to, on } => Some(RelationSnapshot {
                kind: "o2o".into(),
                to: to.to_owned(),
                on: on.to_owned(),
                on_delete: on_delete.clone(),
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
            case_insensitive: f.case_insensitive,
            generated_as: f.generated_as.map(str::to_owned),
            db_comment: f.db_comment.map(str::to_owned),
            fk,
        }
    }
}

pub(crate) fn field_type_name(ty: FieldType) -> &'static str {
    // Like `FieldType::as_str`, but with names that are stable in
    // snapshot JSON.
    match ty {
        FieldType::I16 => "i16",
        FieldType::I32 => "i32",
        FieldType::I64 => "i64",
        FieldType::F32 => "f32",
        FieldType::F64 => "f64",
        FieldType::Bool => "bool",
        FieldType::String => "string",
        FieldType::DateTime => "datetime",
        FieldType::Date => "date",
        FieldType::Time => "time",
        FieldType::Uuid => "uuid",
        FieldType::Json => "json",
        FieldType::Decimal => "decimal",
        FieldType::Binary => "binary",
        FieldType::Array(crate::core::ArrayElem::Text) => "array_text",
        FieldType::Array(crate::core::ArrayElem::Int) => "array_int",
        FieldType::Array(crate::core::ArrayElem::BigInt) => "array_bigint",
        FieldType::Range(crate::core::RangeElem::Int) => "range_int",
        FieldType::Range(crate::core::RangeElem::BigInt) => "range_bigint",
        FieldType::Range(crate::core::RangeElem::Numeric) => "range_numeric",
        FieldType::Range(crate::core::RangeElem::Date) => "range_date",
        FieldType::Range(crate::core::RangeElem::DateTime) => "range_datetime",
        FieldType::HStore => "hstore",
        // The return type is `&'static str`, so the vector dimension
        // is not encoded. A change from `vector(3)` to `vector(4)`
        // does not show up as a schema diff.
        FieldType::Vector(_) => "vector",
        // Same for the geometry SRID: an SRID-only change is not
        // diffed.
        FieldType::Geometry(_) => "geometry",
    }
}

/// Sort by name and keep one entry per name: the smallest definition,
/// not the first in `inventory` order, which a rebuild can change. The
/// diff compares whole definitions, so a flipping winner meant a Drop +
/// Create every run, and for a junction table, lost rows.
fn dedup_by_name<T: PartialOrd + std::fmt::Debug>(
    mut out: Vec<T>,
    kind: &str,
    name: impl Fn(&T) -> &str,
) -> Vec<T> {
    out.sort_by(|a, b| {
        name(a)
            .cmp(name(b))
            .then_with(|| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    });
    out.dedup_by(|later, kept| {
        if name(later) != name(kept) {
            return false;
        }
        if *later != *kept {
            tracing::warn!(
                target: "rustango::migrate",
                name = %name(kept),
                kept = ?kept,
                ignored = ?later,
                "two models declare {kind} `{}` differently; using the first in sort order",
                name(kept),
            );
        }
        true
    });
    out
}

/// Collect all CHECK constraint descriptors, deduplicating by name.
fn collect_checks<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<CheckSnapshot> {
    let mut out: Vec<CheckSnapshot> = Vec::new();
    for schema in schemas {
        for c in schema.check_constraints {
            out.push(CheckSnapshot {
                name: c.name.to_owned(),
                table: schema.table.to_owned(),
                expr: c.expr.to_owned(),
            });
        }
    }
    dedup_by_name(out, "CHECK", |c| &c.name)
}

/// Collect all PG `EXCLUDE` constraints, deduplicated by name.
/// Mirrors [`collect_checks`].
fn collect_excludes<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<ExclusionSnapshot> {
    let mut out: Vec<ExclusionSnapshot> = Vec::new();
    for schema in schemas {
        for x in schema.exclusion_constraints {
            out.push(ExclusionSnapshot {
                name: x.name.to_owned(),
                table: schema.table.to_owned(),
                using: x.using.to_owned(),
                elements: x
                    .elements
                    .iter()
                    .map(|(c, o)| ((*c).to_owned(), (*o).to_owned()))
                    .collect(),
                where_clause: x.where_clause.map(str::to_owned),
            });
        }
    }
    dedup_by_name(out, "EXCLUDE", |x| &x.name)
}

/// Collect all `CREATE INDEX` declarations, deduplicated by name and
/// sorted so the output is stable.
fn collect_indexes<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<IndexSnapshot> {
    let mut out: Vec<IndexSnapshot> = Vec::new();
    for schema in schemas {
        for idx in schema.indexes {
            out.push(IndexSnapshot {
                name: idx.name.to_owned(),
                table: schema.table.to_owned(),
                columns: idx.columns.iter().map(|&c| c.to_owned()).collect(),
                unique: idx.unique,
                method: idx.method.as_str().to_owned(),
                where_clause: idx.where_clause.map(str::to_owned),
                include: idx.include.iter().map(|&c| c.to_owned()).collect(),
            });
        }
    }
    dedup_by_name(out, "index", |i| &i.name)
}

/// Collect all M2M junction tables, deduplicated by `through` name
/// and sorted so the output is stable.
///
/// Relations with `auto_create = false` are skipped: the project owns
/// those junction tables through its own `#[derive(Model)]`, and a
/// second `CREATE TABLE` would clash on apply. The `<name>_m2m()`
/// accessor still works, because it uses the table name rather than
/// the snapshot.
fn collect_m2m_tables<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<M2MTableSnapshot> {
    let out: Vec<M2MTableSnapshot> = schemas.flat_map(m2m_snapshots).collect();
    if let Some(e) = junction_conflict(&out) {
        panic!("{e}");
    }
    dedup_by_name(out, "M2M junction", |m| &m.through)
}

fn m2m_snapshots(schema: &ModelSchema) -> impl Iterator<Item = M2MTableSnapshot> + '_ {
    schema.m2m.iter().filter(|r| r.auto_create).map(|rel| {
        M2MTableSnapshot {
            through: rel.through.to_owned(),
            src_table: schema.table.to_owned(),
            src_col: rel.src_col.to_owned(),
            dst_table: rel.to.to_owned(),
            dst_col: rel.dst_col.to_owned(),
        }
        .canonical()
    })
}

/// One table holds one pair of FKs, so declarations may differ only by
/// which side is the source. Picking one by sort order lost rows (#2000).
fn junction_conflict(out: &[M2MTableSnapshot]) -> Option<String> {
    out.iter().enumerate().find_map(|(i, a)| {
        out[..i]
            .iter()
            .find(|b| b.through == a.through && *b != a)
            .map(|b| {
                format!(
                    "M2M junction `{}` is declared with two shapes: {b:?} and {a:?}; \
                     give each relation its own `through` table",
                    a.through
                )
            })
    })
}

/// `makemigrations` refuses a conflicting junction as an error; the
/// snapshot builders can only panic.
pub(crate) fn check_registry_junctions() -> Result<(), super::MigrateError> {
    let out: Vec<M2MTableSnapshot> = inventory::iter::<ModelEntry>
        .into_iter()
        .filter(|e| !e.schema.is_view && e.schema.managed)
        .flat_map(|e| m2m_snapshots(e.schema))
        .collect();
    junction_conflict(&out).map_or(Ok(()), |e| Err(super::MigrateError::Validation(e)))
}

#[cfg(test)]
mod composite_fk_snapshot_tests {
    use super::*;
    use crate::core::{CompositeFkRelation, FieldSchema, FieldType};

    fn schema_with_composite_fk() -> &'static ModelSchema {
        static FIELDS: [FieldSchema; 1] = [FieldSchema {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
        }];
        static COMPS: [CompositeFkRelation; 1] = [CompositeFkRelation {
            name: "target",
            to: "other_table",
            from: &["a", "b"],
            on: &["x", "y"],
        }];
        static MS: ModelSchema = ModelSchema {
            name: "Demo",
            table: "demo",
            fields: &FIELDS,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            audit_track: None,
            permissions: false,
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            m2m: &[],
            composite_relations: &COMPS,
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        };
        &MS
    }

    #[test]
    fn from_schema_captures_composite_fks_in_declaration_order() {
        let snap = TableSnapshot::from_schema(schema_with_composite_fk());
        assert_eq!(snap.composite_fks.len(), 1);
        let c = &snap.composite_fks[0];
        assert_eq!(c.name, "target");
        assert_eq!(c.to, "other_table");
        assert_eq!(c.from, vec!["a", "b"]);
        assert_eq!(c.on, vec!["x", "y"]);
    }

    /// `#[rustango(managed = false)]` must keep a model out of
    /// `makemigrations`. Driven through `from_models` so nothing has
    /// to be registered in `inventory`, which would leak into every
    /// other test.
    #[test]
    fn unmanaged_models_are_skipped_by_snapshot_from_models() {
        static FIELDS: [FieldSchema; 1] = [FieldSchema {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
        }];
        static MANAGED: ModelSchema = ModelSchema {
            name: "Managed",
            table: "managed_table",
            fields: &FIELDS,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            audit_track: None,
            permissions: false,
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            m2m: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        };
        static UNMANAGED: ModelSchema = ModelSchema {
            name: "Unmanaged",
            table: "unmanaged_table",
            fields: &FIELDS,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            audit_track: None,
            permissions: false,
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            m2m: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: false,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        };
        let snap = SchemaSnapshot::from_models(&[&MANAGED, &UNMANAGED]);
        let table_names: Vec<&str> = snap.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            table_names,
            vec!["managed_table"],
            "unmanaged table must be skipped from the migration snapshot",
        );
    }

    #[test]
    fn empty_composite_fks_skipped_on_serialize_for_back_compat() {
        // A model with no composite FKs must leave the key out, so
        // older snapshot JSON stays diff-clean.
        static FIELDS: [FieldSchema; 1] = [FieldSchema {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            auto_now: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
        }];
        static MS: ModelSchema = ModelSchema {
            name: "Plain",
            table: "plain",
            fields: &FIELDS,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            audit_track: None,
            permissions: false,
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            m2m: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        };
        let snap = TableSnapshot::from_schema(&MS);
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(
            !json.contains("composite_fks"),
            "empty composite_fks should not appear in JSON; got: {json}"
        );
    }

    /// Two models sharing a junction, CHECK, EXCLUDE or index name give
    /// the same snapshot in either `inventory` order.
    #[test]
    fn shared_names_do_not_depend_on_model_order() {
        use crate::core::{CheckConstraint, ExclusionConstraint, IndexSchema, M2MRelation};
        const TAGS: &[M2MRelation] = &[M2MRelation::new(
            "tags",
            "tag",
            "shared_tags",
            "post_id",
            "tag_id",
        )];
        // The same junction declared from the other side.
        const POSTS: &[M2MRelation] = &[M2MRelation::new(
            "posts",
            "post",
            "shared_tags",
            "tag_id",
            "post_id",
        )];
        const CK: &[CheckConstraint] = &[CheckConstraint::new("shared_ck", "id > 0")];
        const EX: &[ExclusionConstraint] = &[ExclusionConstraint::new(
            "shared_ex",
            "gist",
            &[("id", "=")],
        )];
        const IX: &[IndexSchema] = &[IndexSchema::new("shared_ix", &["id"])];
        const fn model(
            name: &'static str,
            table: &'static str,
            m2m: &'static [M2MRelation],
        ) -> ModelSchema {
            let mut s = ModelSchema::new(name, table);
            s.m2m = m2m;
            s.check_constraints = CK;
            s.exclusion_constraints = EX;
            s.indexes = IX;
            s
        }
        static POST: ModelSchema = model("Post", "post", TAGS);
        static TAG: ModelSchema = model("Tag", "tag", POSTS);
        let a = SchemaSnapshot::from_models(&[&POST, &TAG]);
        let b = SchemaSnapshot::from_models(&[&TAG, &POST]);
        assert_eq!(a, b);
        assert_eq!(a.m2m_tables.len(), 1);
        assert_eq!(a.m2m_tables[0].src_table, "post");
        assert_eq!(
            (a.checks.len(), a.excludes.len(), a.indexes.len()),
            (1, 1, 1)
        );
    }

    /// A new model that sorts first and reuses a junction must not take it over.
    #[test]
    #[should_panic(expected = "M2M junction `shared_tags` is declared with two shapes")]
    fn a_second_junction_shape_is_refused() {
        use crate::core::M2MRelation;
        const TAGS: &[M2MRelation] = &[M2MRelation::new(
            "tags",
            "tag",
            "shared_tags",
            "post_id",
            "tag_id",
        )];
        const fn model(name: &'static str, table: &'static str) -> ModelSchema {
            let mut s = ModelSchema::new(name, table);
            s.m2m = TAGS;
            s
        }
        static POST: ModelSchema = model("Post", "post");
        static ARTICLE: ModelSchema = model("Article", "article");
        let _ = SchemaSnapshot::from_models(&[&POST, &ARTICLE]);
    }
}

#[cfg(test)]
mod generated_as_and_db_comment_capture {
    use super::*;
    use crate::core::{FieldSchema, FieldType};

    fn schema_with_generated_and_comment() -> &'static ModelSchema {
        static FIELDS: [FieldSchema; 3] = [
            FieldSchema {
                name: "id",
                column: "id",
                ty: FieldType::I64,
                nullable: false,
                primary_key: true,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: true,
                auto_now: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                case_insensitive: false,
                fk_on_delete: None,
                validators: &[],
            },
            FieldSchema {
                name: "total",
                column: "total",
                ty: FieldType::F64,
                nullable: false,
                primary_key: false,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: false,
                auto_now: false,
                unique: false,
                generated_as: Some("price * quantity"),
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                case_insensitive: false,
                fk_on_delete: None,
                validators: &[],
            },
            FieldSchema {
                name: "label",
                column: "label",
                ty: FieldType::String,
                nullable: false,
                primary_key: false,
                relation: None,
                max_length: Some(64),
                min: None,
                max: None,
                default: None,
                auto: false,
                auto_now: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: Some("Human-readable label."),
                verbose_name: None,
                editable: true,
                blank: false,
                case_insensitive: false,
                fk_on_delete: None,
                validators: &[],
            },
        ];
        static MS: ModelSchema = ModelSchema {
            name: "Item",
            table: "items",
            fields: &FIELDS,
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            audit_track: None,
            permissions: false,
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            m2m: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        };
        &MS
    }

    #[test]
    fn snapshot_captures_generated_as() {
        let snap = TableSnapshot::from_schema(schema_with_generated_and_comment());
        let total = snap
            .fields
            .iter()
            .find(|f| f.column == "total")
            .expect("total field");
        assert_eq!(total.generated_as.as_deref(), Some("price * quantity"));
    }

    #[test]
    fn snapshot_captures_db_comment() {
        let snap = TableSnapshot::from_schema(schema_with_generated_and_comment());
        let label = snap
            .fields
            .iter()
            .find(|f| f.column == "label")
            .expect("label field");
        assert_eq!(label.db_comment.as_deref(), Some("Human-readable label."));
    }

    #[test]
    fn snapshot_serializes_generated_and_comment_back() {
        let snap = TableSnapshot::from_schema(schema_with_generated_and_comment());
        let json = serde_json::to_string(&snap).expect("serialize");
        // generated_as captured for the `total` column.
        assert!(
            json.contains(r#""generated_as":"price * quantity""#),
            "expected generated_as in JSON: {json}"
        );
        // db_comment captured for the `label` column.
        assert!(
            json.contains(r#""db_comment":"Human-readable label.""#),
            "expected db_comment in JSON: {json}"
        );
        // Round-trips through serde.
        let back: TableSnapshot = serde_json::from_str(&json).expect("deserialize");
        let total = back.fields.iter().find(|f| f.column == "total").unwrap();
        assert_eq!(total.generated_as.as_deref(), Some("price * quantity"));
        let label = back.fields.iter().find(|f| f.column == "label").unwrap();
        assert_eq!(label.db_comment.as_deref(), Some("Human-readable label."));
    }

    #[test]
    fn snapshot_skips_serializing_when_none() {
        let snap = TableSnapshot::from_schema(schema_with_generated_and_comment());
        let json = serde_json::to_string(&snap).expect("serialize");
        // `id` sets neither field, so neither key may appear in its
        // JSON object. Re-parse and check.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let fields = value.get("fields").unwrap().as_array().unwrap();
        let id_field = fields
            .iter()
            .find(|f| f.get("column").and_then(|c| c.as_str()) == Some("id"))
            .unwrap();
        assert!(
            id_field.get("generated_as").is_none(),
            "id field should not have generated_as key"
        );
        assert!(
            id_field.get("db_comment").is_none(),
            "id field should not have db_comment key"
        );
    }
}
