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
    /// CHECK constraints derived from `ModelSchema::check_constraints`,
    /// sorted by name. Absent from old migration files — defaults to empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckSnapshot>,
    /// Postgres `EXCLUDE` constraints derived from
    /// `ModelSchema::exclusion_constraints`, sorted by name. PG-only —
    /// the migration writer renders nothing on MySQL/SQLite (with a
    /// `tracing::warn!`). Absent from pre-#319 migration files —
    /// defaults to empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<ExclusionSnapshot>,
}

/// Snapshot of one Postgres `EXCLUDE` constraint. Issue #319.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckSnapshot {
    pub name: String,
    pub table: String,
    pub expr: String,
}

/// Snapshot of one `CREATE INDEX` declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexSnapshot {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    /// Access method as a lowercase token (`btree` / `gin` / `gist` /
    /// `brin` / `spgist` / `hash` / `bloom`). Defaults to `"btree"`
    /// when missing or unknown — keeps pre-#34 snapshots forward-
    /// compatible (older files have no `method` key; deserialize
    /// fills it with the default + the migration runner emits
    /// regular btree).
    #[serde(default = "default_index_method")]
    pub method: String,
    /// Optional partial-index `WHERE <expr>` clause — Django's
    /// `UniqueConstraint(condition=Q(...))`. Issue #265 / T1.3.
    /// `None` (default) emits a plain index. Older snapshots without
    /// this key deserialize cleanly via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub where_clause: Option<String>,
    /// Django `Index(include=[...])` covering-index columns. PG 11+
    /// only; MySQL/SQLite drop the clause at render time with a
    /// warning. Empty `Vec` (default) means "no covering columns".
    /// Older snapshots without this key deserialize cleanly via
    /// `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
}

fn default_index_method() -> String {
    "btree".to_owned()
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
    /// Composite (multi-column) FKs declared on the model via
    /// `#[rustango(fk_composite(...))]`. Sub-slice F.5 of the
    /// v0.15.0 ContentType plan. Skipped on serialize when empty
    /// so older snapshots written before F.5 stay diff-clean
    /// (matches the `m2m_tables` / `indexes` / `checks`
    /// already-empty-elision pattern).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub composite_fks: Vec<CompositeFkSnapshot>,
}

/// Serialized form of [`crate::core::CompositeFkRelation`]. Captured
/// per-table in [`TableSnapshot`]. Stable order: declaration order
/// from the `#[rustango(fk_composite(...))]` attrs on the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeFkSnapshot {
    /// Logical relation name (free-form Rust identifier).
    pub name: String,
    /// Target SQL table name.
    pub to: String,
    /// Source-side column names, in declaration order.
    pub from: Vec<String>,
    /// Target-side column names, same length / order as `from`.
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
    /// `true` for fields whose Rust type is `Auto<T>` — server-assigned
    /// PKs that translate to `BIGSERIAL` / `SERIAL` in DDL. Skipped on
    /// serialize when `false` so older snapshots stay diff-clean.
    #[serde(skip_serializing_if = "is_false", default)]
    pub auto: bool,
    /// `true` when `#[rustango(unique)]` was declared. Skipped on
    /// serialize when `false` to keep snapshots diff-clean.
    #[serde(skip_serializing_if = "is_false", default)]
    pub unique: bool,
    /// Django-shape `CITextField` flag (#344). Threaded through from
    /// `FieldSchema::case_insensitive`. Skipped on serialize when
    /// `false` so older snapshots stay diff-clean. The diff writer
    /// dispatches to `dialect.ci_text_type(max_length)` when set.
    #[serde(skip_serializing_if = "is_false", default)]
    pub case_insensitive: bool,
    /// Raw SQL expression for a `GENERATED ALWAYS AS (...) STORED`
    /// computed column. Threaded through from
    /// `FieldSchema::generated_as`. Skipped on serialize when `None`
    /// so older snapshots stay diff-clean. Captured in #559 — was
    /// previously dropped from file-based migrations, causing
    /// generated columns to silently disappear when applying from
    /// snapshot JSON instead of the live registry.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generated_as: Option<String>,
    /// Django-shape `db_comment="..."` column comment. Threaded
    /// through from `FieldSchema::db_comment`. Skipped on serialize
    /// when `None`. Captured in #559 — was previously dropped from
    /// file-based migrations so MySQL/PG `COMMENT` clauses were
    /// missing when applying from snapshot JSON.
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
}

/// Was this model registered by the framework itself (as opposed to a
/// downstream crate)? Used to decide which model owns a `rustango_*` table
/// when a project overrides one — see [`SchemaSnapshot::from_registry_system_for_scope`].
fn is_framework_model(e: &ModelEntry) -> bool {
    e.module_path == "rustango" || e.module_path.starts_with("rustango::")
}

impl SchemaSnapshot {
    /// Capture every model registered in the binary's `inventory`.
    ///
    /// Issue #293 / T2.10 — models marked `#[rustango(view)]` are
    /// excluded. Their underlying SQL view is operator-owned, not
    /// rustango-owned, so the diff machinery must never emit
    /// `CREATE TABLE` / `DROP TABLE` against them.
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

    /// Capture only the models whose [`crate::core::ModelSchema::scope`]
    /// matches `scope`. Powers tenancy-aware `makemigrations` — registry
    /// changes (e.g. `rustango_orgs`, `rustango_operators`) and tenant
    /// changes (everything else) get partitioned into separate migration
    /// files, each tagged with the matching [`super::MigrationScope`].
    ///
    /// Without this split, framework registry-scoped models are diff'd
    /// alongside tenant ones into a single tenant-scoped migration —
    /// when the migration replays under a tenant's `search_path`, ALTERs
    /// for `rustango_operators` resolve to the registry copy and crash
    /// (`relation … already exists`).
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

    /// Capture only the **framework** models (reserved `rustango_`
    /// table-name namespace) whose scope matches `scope`. This is the
    /// "system app" — the framework's own tables, which `makemigrations`
    /// generates into the project's `system/migrations/` folder instead
    /// of hand-written bootstrap/ensure DDL. User (non-`rustango_`)
    /// models are excluded so the framework's migrations and the app's
    /// own migrations never mix.
    #[must_use]
    pub fn from_registry_system_for_scope(scope: crate::core::ModelScope) -> Self {
        // A few framework tables are "shared" — they exist in EVERY
        // rustango database (the registry AND each tenant), because both
        // do audits / carry a content-type catalog. The single-scope
        // migration model can't say "both", so they're included in every
        // scope's system migrations (each DB creates its own copy).
        const SHARED: &[&str] = &["rustango_audit_log", "rustango_content_types"];
        let matching = inventory::iter::<ModelEntry>.into_iter().filter(|e| {
            (e.schema.scope == scope || SHARED.contains(&e.schema.table))
                && e.schema.table.starts_with("rustango_")
                && !e.schema.is_view
                && e.schema.managed
        });
        // Exactly one model may own a table. The documented custom-user-model
        // path has a downstream model declare `rustango_users` with extra
        // columns — but the built-in `User` derive is unconditional, so both
        // land in the inventory and both used to flow into the migration,
        // yielding a duplicate/ambiguous `CREATE TABLE` (#1168). Dedup by
        // table, and let the *downstream* model win: a project that spells out
        // a framework table is overriding it on purpose.
        let mut by_table: std::collections::BTreeMap<&'static str, &ModelEntry> =
            std::collections::BTreeMap::new();
        for e in matching {
            match by_table.entry(e.schema.table) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(e);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    let held_is_framework = is_framework_model(slot.get());
                    if held_is_framework && !is_framework_model(e) {
                        slot.insert(e);
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

    /// Filter `self` to only the tables / indexes / checks whose owning
    /// model has [`crate::core::ModelSchema::scope`] matching `scope`.
    /// Used to filter a *prior* on-disk snapshot down to one scope before
    /// diffing — necessary because the framework's bootstrap migrations
    /// (and v0.23.x and earlier projects) put ALL framework tables into
    /// the same snapshot regardless of which scope the migration ran in.
    ///
    /// Tables not currently in the inventory (model removed since the
    /// snapshot was written) default to [`ModelScope::Tenant`] —
    /// matches `MigrationScope`'s default and never accidentally
    /// promotes a vanished registry table back into a tenant migration.
    #[must_use]
    pub fn filtered_to_scope(&self, scope: crate::core::ModelScope) -> Self {
        let scope_of = |table: &str| {
            inventory::iter::<ModelEntry>
                .into_iter()
                .find(|e| e.schema.table == table)
                .map_or(crate::core::ModelScope::Tenant, |e| e.schema.scope)
        };
        let tables: Vec<TableSnapshot> = self
            .tables
            .iter()
            .filter(|t| scope_of(&t.name) == scope)
            .cloned()
            .collect();
        // M2M / indexes / checks live on a parent table — keep only
        // those whose parent is in `tables`.
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

    /// Capture an explicit list of model schemas — the inventory-
    /// agnostic counterpart of [`from_registry`]. Used by callers that
    /// want a curated snapshot rather than every linked model (e.g.
    /// `rustango-tenancy`'s bootstrap migrations, which pin themselves
    /// to `rustango_orgs` + `rustango_operators` + `rustango_users`).
    ///
    /// View-backed models (`#[rustango(view)]`) are filtered out — same
    /// rule as [`from_registry`].
    #[must_use]
    pub fn from_models(models: &[&ModelSchema]) -> Self {
        Self::build_from(models.iter().copied().filter(|s| !s.is_view && s.managed))
    }

    /// Like [`Self::from_models`] but does **not** skip `managed = false`
    /// models. The drift-free `ensure_*` table helpers use this: their
    /// models are intentionally `managed = false` (outside the migration
    /// set — the framework creates them lazily on first use), yet the
    /// helper still needs to render the table's `CREATE TABLE` from
    /// [`ModelSchema`] rather than hand-written per-dialect DDL. Views are
    /// still skipped.
    #[must_use]
    pub fn from_models_forced(models: &[&ModelSchema]) -> Self {
        Self::build_from(models.iter().copied().filter(|s| !s.is_view))
    }

    /// Shared body of [`Self::from_models`] / [`Self::from_models_forced`]:
    /// builds the snapshot from an already-filtered model iterator.
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
    /// Build a snapshot row from a registered [`ModelSchema`]. Public
    /// so external callers (e.g. tenancy bootstrap migrations) can
    /// assemble their own snapshots without going through the global
    /// inventory.
    #[must_use]
    pub fn from_schema(s: &ModelSchema) -> Self {
        let mut fields: Vec<FieldSnapshot> =
            s.scalar_fields().map(FieldSnapshot::from_schema).collect();
        fields.sort_by(|a, b| a.column.cmp(&b.column));
        // Composite FK relations (sub-slice F.5) — preserve declaration
        // order so snapshot diffs aren't sensitive to a meaningless
        // reorder. Empty Vec for models with no fk_composite attrs;
        // the field's `skip_serializing_if = Vec::is_empty` keeps
        // pre-F.5 snapshot JSON byte-identical.
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
            case_insensitive: f.case_insensitive,
            generated_as: f.generated_as.map(str::to_owned),
            db_comment: f.db_comment.map(str::to_owned),
            fk,
        }
    }
}

pub(crate) fn field_type_name(ty: FieldType) -> &'static str {
    // Reuse the `FieldType::as_str` mapping but with stable JSON names.
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
        // #341 — stable snapshot names for PG array element kinds.
        FieldType::Array(crate::core::ArrayElem::Text) => "array_text",
        FieldType::Array(crate::core::ArrayElem::Int) => "array_int",
        FieldType::Array(crate::core::ArrayElem::BigInt) => "array_bigint",
        // #343 — stable snapshot names for PG range element kinds.
        FieldType::Range(crate::core::RangeElem::Int) => "range_int",
        FieldType::Range(crate::core::RangeElem::BigInt) => "range_bigint",
        FieldType::Range(crate::core::RangeElem::Numeric) => "range_numeric",
        FieldType::Range(crate::core::RangeElem::Date) => "range_date",
        FieldType::Range(crate::core::RangeElem::DateTime) => "range_datetime",
        // #342 — PG hstore.
        FieldType::HStore => "hstore",
        // #824 — pgvector. NOTE: dims aren't encoded here (this returns
        // `&'static str`), so a dimension-only change (`vector(3)` →
        // `vector(4)`) isn't surfaced as a schema diff.
        FieldType::Vector(_) => "vector",
        // #443 — PostGIS geometry. Like `vector`, the SRID isn't encoded
        // in this `&'static str`, so an SRID-only change isn't diffed.
        FieldType::Geometry(_) => "geometry",
    }
}

/// Collect all CHECK constraint descriptors, deduplicating by name.
fn collect_checks<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<CheckSnapshot> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<CheckSnapshot> = Vec::new();
    for schema in schemas {
        for c in schema.check_constraints {
            if seen.insert(c.name) {
                out.push(CheckSnapshot {
                    name: c.name.to_owned(),
                    table: schema.table.to_owned(),
                    expr: c.expr.to_owned(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Collect all PG `EXCLUDE` constraint descriptors, deduplicating by
/// name. Mirrors [`collect_checks`]. Issue #319.
fn collect_excludes<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<ExclusionSnapshot> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<ExclusionSnapshot> = Vec::new();
    for schema in schemas {
        for x in schema.exclusion_constraints {
            if seen.insert(x.name) {
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
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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
                    method: idx.method.as_str().to_owned(),
                    where_clause: idx.where_clause.map(str::to_owned),
                    include: idx.include.iter().map(|&c| c.to_owned()).collect(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Collect all M2M junction table descriptors from a set of model schemas,
/// deduplicating by `through` table name and sorting for deterministic output.
///
/// #324 — relations marked `auto_create = false` are skipped here.
/// The operator owns those junction tables via their own
/// `#[derive(Model)]` struct, so the migration writer must not emit a
/// second `CREATE TABLE` for them (which would conflict on apply).
/// The Rust-side `<name>_m2m()` accessor still works because it
/// references the through table by name, not by snapshot.
fn collect_m2m_tables<'a>(schemas: impl Iterator<Item = &'a ModelSchema>) -> Vec<M2MTableSnapshot> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<M2MTableSnapshot> = Vec::new();
    for schema in schemas {
        for rel in schema.m2m {
            if !rel.auto_create {
                continue;
            }
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

    /// Issue #321 — `#[rustango(managed = false)]` keeps the model
    /// out of `makemigrations` output. We exercise the registry path
    /// through `SchemaSnapshot::from_models` so we don't need to register
    /// real models into `inventory` (which would leak into every
    /// downstream test).
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
        // Models without composite FKs serialize without the
        // composite_fks field — pre-F.5 snapshot JSON stays
        // diff-clean against post-F.5 builds.
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
        // The `id` column has neither generated_as nor db_comment set —
        // it should NOT have those keys in JSON (skip_serializing_if).
        // We check that the `id` field's JSON object doesn't have the
        // keys by re-parsing.
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
