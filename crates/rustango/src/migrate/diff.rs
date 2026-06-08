//! Diff two `SchemaSnapshot`s into a list of DDL statements.
//!
//! v0.2 scope: detect new tables, dropped tables, new columns, dropped
//! columns. Type / constraint changes and renames are explicitly
//! deferred — they can't be inferred from a snapshot diff (rename vs
//! drop+add are indistinguishable) and need a more explicit migration
//! authoring story (Django's `RenameField` operation).
//!
//! Output is `Vec<String>` of fully-formed Postgres DDL the runner can
//! execute one statement at a time. New-table CREATE TABLEs come before
//! ADD COLUMNs (so a new table referenced by a new column already
//! exists), and DROP COLUMNs come before DROP TABLEs for the same
//! reason. FK constraints for new tables are emitted last.
//!
//! `ADD COLUMN ... NOT NULL` is supported only when the field carries
//! a `default` (rendered as `DEFAULT <expr>` so Postgres can backfill
//! existing rows). Without a default, `AddColumn` of a non-null field
//! is rejected with an explanatory error pointing at the two fixes:
//! make the field `Option<T>`, or set `#[rustango(default = "…")]`.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::snapshot::{FieldSnapshot, SchemaSnapshot, TableSnapshot};

/// One thing that should change to move from `prev` to `current`.
///
/// Serializes externally-tagged: `{"CreateTable": "foo"}`,
/// `{"AddColumn": {"table": "foo", "column": "bar"}}`. That's what
/// migration files store under `Operation::Schema`.
fn default_index_method_diff() -> String {
    "btree".to_owned()
}

fn default_exclusion_method() -> String {
    "gist".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaChange {
    CreateTable(String /* table name */),
    DropTable(String /* table name */),
    AddColumn {
        table: String,
        column: String,
    },
    DropColumn {
        table: String,
        column: String,
    },
    /// Change a column's underlying type — `i32 → i64`, `String → Uuid`, etc.
    /// Carried as the dialect-neutral name string (matches `FieldSnapshot.ty`
    /// rather than the closed `FieldType` enum so externally-supplied
    /// migration files don't break when v0.4+ adds new types). Render emits
    /// `ALTER TABLE ... ALTER COLUMN ... TYPE <pg_type> USING <col>::<pg_type>`.
    AlterColumnType {
        table: String,
        column: String,
        from: String,
        to: String,
    },
    /// Toggle a column between nullable and NOT NULL. `nullable` is the
    /// **new** state. Render emits `SET NOT NULL` (when false) or
    /// `DROP NOT NULL` (when true).
    AlterColumnNullable {
        table: String,
        column: String,
        nullable: bool,
    },
    /// Change a column's `DEFAULT` clause. `Some(expr)` sets the default
    /// to the given Postgres expression; `None` drops the default.
    /// `from`/`to` is enough to invert without consulting a snapshot.
    AlterColumnDefault {
        table: String,
        column: String,
        from: Option<String>,
        to: Option<String>,
    },
    /// Change a String column's `max_length` (VARCHAR(N) ↔ TEXT, or
    /// between two VARCHAR sizes). Render emits `TYPE VARCHAR(N)` or
    /// `TYPE TEXT` accordingly.
    AlterColumnMaxLength {
        table: String,
        column: String,
        from: Option<u32>,
        to: Option<u32>,
    },
    /// Rename a table. Not emitted by `detect_changes` — rename vs
    /// drop+add is ambiguous from a snapshot diff (Django's reasoning).
    /// Authored manually via `manage makemigrations --empty <name>`
    /// then editing the JSON.
    RenameTable {
        old_name: String,
        new_name: String,
    },
    /// Rename a column. Same authoring constraint as `RenameTable`.
    RenameColumn {
        table: String,
        old_column: String,
        new_column: String,
    },
    /// Add or drop a `UNIQUE` constraint on a single column.
    /// `unique` is the **new** state. Render emits
    /// `ADD CONSTRAINT … UNIQUE` or `DROP CONSTRAINT`.
    AlterColumnUnique {
        table: String,
        column: String,
        unique: bool,
    },
    /// Create a `CREATE [UNIQUE] INDEX` on a model table.
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        /// Access method as a lowercase token (`btree` / `gin` /
        /// `gist` / …). Defaults to `"btree"` when absent — keeps
        /// migration JSON files written before issue #34 forward-
        /// compatible.
        #[serde(default = "default_index_method_diff")]
        method: String,
        /// Optional partial-index `WHERE` clause — Django
        /// `UniqueConstraint(condition=Q(...))`. Issue #265 / T1.3.
        /// `None` for plain indexes; `Some(expr)` emits
        /// `CREATE UNIQUE INDEX ... WHERE <expr>` on PG / SQLite.
        /// MySQL has no partial-index syntax — the writer drops the
        /// WHERE clause with a doc-level warning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_clause: Option<String>,
        /// Django `Index(include=[...])` covering-index columns. PG
        /// 11+ ships `INCLUDE (...)` syntax — non-key columns travel
        /// with the index leaf for index-only scans. MySQL/SQLite
        /// lack it; the renderer drops the clause with a warning.
        /// Empty `Vec` (the default) means "no covering columns".
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include: Vec<String>,
    },
    /// Drop an index by name.
    DropIndex {
        name: String,
    },
    /// Add a table-level CHECK constraint.
    AddCheckConstraint {
        name: String,
        table: String,
        expr: String,
    },
    /// Drop a CHECK constraint by name.
    DropCheckConstraint {
        name: String,
        table: String,
    },
    /// Add a Postgres `EXCLUDE` constraint — Django's
    /// `ExclusionConstraint`. **PG-only** (MySQL + SQLite have no
    /// equivalent; render emits nothing + logs a warning so the
    /// rest of the migration still applies). Issue #32.
    ///
    /// Renders as `ALTER TABLE <table> ADD CONSTRAINT <name>
    /// EXCLUDE USING <method> (<elements>) [WHERE (<where>)]`,
    /// where each element is a `(column, operator)` pair like
    /// `("room_id", "=")` or `("during", "&&")`. The canonical
    /// shape for booking-conflict prevention is:
    /// `EXCLUDE USING gist (room_id WITH =, during WITH &&)`.
    AddExclusionConstraint {
        name: String,
        table: String,
        /// Index method (`gist` / `btree_gist` / `spgist`). Defaults
        /// to `gist` when absent — most exclusion constraints rely
        /// on GiST's range-overlap support.
        #[serde(default = "default_exclusion_method")]
        using: String,
        /// `(column, operator)` pairs in declaration order. The
        /// operator is the PG comparison op for that column —
        /// usually `=` for equality columns, `&&` for range
        /// overlap, `@>` for containment.
        elements: Vec<(String, String)>,
        /// Optional `WHERE` predicate that narrows the constraint
        /// to a subset of rows (e.g. only active bookings).
        /// `None` = unconditional.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_clause: Option<String>,
    },
    /// Drop an `EXCLUDE` constraint by name. PG-only. Issue #32.
    DropExclusionConstraint {
        name: String,
        table: String,
    },
    /// Create a many-to-many junction table. Render emits a `CREATE TABLE`
    /// with two `BIGINT NOT NULL` FK columns and a composite `PRIMARY KEY`.
    CreateM2MTable {
        through: String,
        src_table: String,
        src_col: String,
        dst_table: String,
        dst_col: String,
    },
    /// Drop a many-to-many junction table.
    DropM2MTable {
        through: String,
    },
    /// Add a composite (multi-column) foreign-key constraint declared
    /// via `#[rustango(fk_composite(...))]`. Sub-slice F.5b. Render
    /// emits `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY (...)
    /// REFERENCES ...(...)` and routes the statement through
    /// `deferred_fks` so the referenced table exists by the time the
    /// constraint is created.
    AddCompositeFk {
        table: String,
        name: String,
        to: String,
        from: Vec<String>,
        on: Vec<String>,
    },
    /// Drop a composite FK by constraint name. Render emits
    /// `ALTER TABLE ... DROP CONSTRAINT IF EXISTS ...`.
    DropCompositeFk {
        table: String,
        name: String,
    },
}

/// Compute the ordered list of changes from `prev` → `current`.
///
/// Order:
/// 1. `CreateTable` (new tables)
/// 2. `AddColumn` (new columns on existing tables)
/// 3. `AlterColumn*` (metadata changes on same-named columns)
/// 4. `DropColumn` (dropped columns on remaining tables)
/// 5. `DropTable` (dropped tables)
///
/// Renames (`RenameTable`, `RenameColumn`) are **never** emitted by
/// `detect_changes` — rename vs drop+add is ambiguous from a
/// snapshot diff (Django's reasoning). Authors hand-write rename
/// migrations via `manage makemigrations --empty <name>` and edit
/// the JSON directly. Likewise, FK/PK/CHECK changes still surface
/// the v0.3.1 polish #3 hard error today; full FK/CHECK alters land
/// in a follow-up.
#[must_use]
pub fn detect_changes(prev: &SchemaSnapshot, current: &SchemaSnapshot) -> Vec<SchemaChange> {
    let mut changes = Vec::new();

    // New tables.
    for t in &current.tables {
        if prev.table(&t.name).is_none() {
            changes.push(SchemaChange::CreateTable(t.name.clone()));
        }
    }
    // New columns on existing tables.
    for t in &current.tables {
        let Some(pt) = prev.table(&t.name) else {
            continue;
        };
        for f in &t.fields {
            if pt.field(&f.column).is_none() {
                changes.push(SchemaChange::AddColumn {
                    table: t.name.clone(),
                    column: f.column.clone(),
                });
            }
        }
    }
    // Metadata changes on same-named columns. Replaces the v0.3.1
    // polish hard error: type/nullable/default/max_length changes
    // now produce concrete AlterColumn ops instead of bailing.
    for ct in &current.tables {
        let Some(pt) = prev.table(&ct.name) else {
            continue;
        };
        for cf in &ct.fields {
            let Some(pf) = pt.field(&cf.column) else {
                continue;
            };
            push_alter_changes(&ct.name, pf, cf, &mut changes);
        }
    }
    // Dropped columns on remaining tables.
    for pt in &prev.tables {
        let Some(t) = current.table(&pt.name) else {
            continue;
        };
        for f in &pt.fields {
            if t.field(&f.column).is_none() {
                changes.push(SchemaChange::DropColumn {
                    table: pt.name.clone(),
                    column: f.column.clone(),
                });
            }
        }
    }
    // Dropped tables.
    for pt in &prev.tables {
        if current.table(&pt.name).is_none() {
            changes.push(SchemaChange::DropTable(pt.name.clone()));
        }
    }
    // New indexes — present in current, absent from prev.
    for idx in &current.indexes {
        if prev.index(&idx.name).is_none() {
            changes.push(SchemaChange::CreateIndex {
                name: idx.name.clone(),
                table: idx.table.clone(),
                columns: idx.columns.clone(),
                unique: idx.unique,
                method: idx.method.clone(),
                where_clause: idx.where_clause.clone(),
                include: idx.include.clone(),
            });
        }
    }
    // Dropped indexes — present in prev, absent from current.
    for idx in &prev.indexes {
        if current.index(&idx.name).is_none() {
            changes.push(SchemaChange::DropIndex {
                name: idx.name.clone(),
            });
        }
    }
    // Changed indexes — same name in both, but columns / table /
    // unique flag differ. Without this branch, a model edit that
    // tweaks an index in place (without renaming it) would land a
    // silently-stale index in the database. The diff lowers each
    // such change to a Drop + Create pair so the new shape is
    // applied atomically.
    for idx in &current.indexes {
        if let Some(prev_idx) = prev.index(&idx.name) {
            if prev_idx.columns != idx.columns
                || prev_idx.unique != idx.unique
                || prev_idx.table != idx.table
                || prev_idx.method != idx.method
                || prev_idx.where_clause != idx.where_clause
            {
                changes.push(SchemaChange::DropIndex {
                    name: idx.name.clone(),
                });
                changes.push(SchemaChange::CreateIndex {
                    name: idx.name.clone(),
                    table: idx.table.clone(),
                    columns: idx.columns.clone(),
                    unique: idx.unique,
                    method: idx.method.clone(),
                    where_clause: idx.where_clause.clone(),
                    include: idx.include.clone(),
                });
            }
        }
    }
    // Also detect include-only changes so a fresh covering set
    // re-emits as Drop + Create.
    for idx in &current.indexes {
        if let Some(prev_idx) = prev.index(&idx.name) {
            if prev_idx.columns == idx.columns
                && prev_idx.unique == idx.unique
                && prev_idx.table == idx.table
                && prev_idx.method == idx.method
                && prev_idx.where_clause == idx.where_clause
                && prev_idx.include != idx.include
            {
                changes.push(SchemaChange::DropIndex {
                    name: idx.name.clone(),
                });
                changes.push(SchemaChange::CreateIndex {
                    name: idx.name.clone(),
                    table: idx.table.clone(),
                    columns: idx.columns.clone(),
                    unique: idx.unique,
                    method: idx.method.clone(),
                    where_clause: idx.where_clause.clone(),
                    include: idx.include.clone(),
                });
            }
        }
    }
    // New CHECK constraints.
    for c in &current.checks {
        if prev.check(&c.name).is_none() {
            changes.push(SchemaChange::AddCheckConstraint {
                name: c.name.clone(),
                table: c.table.clone(),
                expr: c.expr.clone(),
            });
        }
    }
    // Dropped CHECK constraints.
    for c in &prev.checks {
        if current.check(&c.name).is_none() {
            changes.push(SchemaChange::DropCheckConstraint {
                name: c.name.clone(),
                table: c.table.clone(),
            });
        }
    }
    // New PG EXCLUDE constraints (issue #319). Dropped EXCLUDEs surface
    // only when the constraint name disappears from the model — the
    // migration writer emits a `DropExclusionConstraint`. Same posture
    // as CHECK: we never rewrite an existing constraint, only add and
    // drop. To change one, operator drops + re-adds via the next
    // makemigrations cycle.
    let prev_exclude_names: std::collections::HashSet<&str> =
        prev.excludes.iter().map(|x| x.name.as_str()).collect();
    let current_exclude_names: std::collections::HashSet<&str> =
        current.excludes.iter().map(|x| x.name.as_str()).collect();
    for x in &current.excludes {
        if !prev_exclude_names.contains(x.name.as_str()) {
            changes.push(SchemaChange::AddExclusionConstraint {
                name: x.name.clone(),
                table: x.table.clone(),
                using: x.using.clone(),
                elements: x.elements.clone(),
                where_clause: x.where_clause.clone(),
            });
        }
    }
    for x in &prev.excludes {
        if !current_exclude_names.contains(x.name.as_str()) {
            changes.push(SchemaChange::DropExclusionConstraint {
                name: x.name.clone(),
                table: x.table.clone(),
            });
        }
    }
    // New M2M junction tables.
    for mt in &current.m2m_tables {
        if prev.m2m_table(&mt.through).is_none() {
            changes.push(SchemaChange::CreateM2MTable {
                through: mt.through.clone(),
                src_table: mt.src_table.clone(),
                src_col: mt.src_col.clone(),
                dst_table: mt.dst_table.clone(),
                dst_col: mt.dst_col.clone(),
            });
        }
    }
    // Dropped M2M junction tables.
    for mt in &prev.m2m_tables {
        if current.m2m_table(&mt.through).is_none() {
            changes.push(SchemaChange::DropM2MTable {
                through: mt.through.clone(),
            });
        }
    }
    // New composite FK constraints (added on existing tables, or on
    // brand-new tables — we emit them either way and let `render`
    // route through `deferred_fks` so referenced tables exist first).
    for ct in &current.tables {
        let prev_fks: &[_] = prev
            .table(&ct.name)
            .map(|t| t.composite_fks.as_slice())
            .unwrap_or(&[]);
        for cf in &ct.composite_fks {
            if !prev_fks.iter().any(|p| p.name == cf.name) {
                changes.push(SchemaChange::AddCompositeFk {
                    table: ct.name.clone(),
                    name: cf.name.clone(),
                    to: cf.to.clone(),
                    from: cf.from.clone(),
                    on: cf.on.clone(),
                });
            }
        }
    }
    // Dropped composite FK constraints (still-present tables only —
    // a `DropTable` already cascades the constraint).
    for pt in &prev.tables {
        let Some(ct) = current.table(&pt.name) else {
            continue;
        };
        for pf in &pt.composite_fks {
            if !ct.composite_fks.iter().any(|c| c.name == pf.name) {
                changes.push(SchemaChange::DropCompositeFk {
                    table: pt.name.clone(),
                    name: pf.name.clone(),
                });
            }
        }
    }
    changes
}

fn push_alter_changes(
    table: &str,
    pf: &FieldSnapshot,
    cf: &FieldSnapshot,
    out: &mut Vec<SchemaChange>,
) {
    if pf.ty != cf.ty {
        out.push(SchemaChange::AlterColumnType {
            table: table.to_owned(),
            column: cf.column.clone(),
            from: pf.ty.clone(),
            to: cf.ty.clone(),
        });
    }
    if pf.nullable != cf.nullable {
        out.push(SchemaChange::AlterColumnNullable {
            table: table.to_owned(),
            column: cf.column.clone(),
            nullable: cf.nullable,
        });
    }
    if pf.default != cf.default {
        out.push(SchemaChange::AlterColumnDefault {
            table: table.to_owned(),
            column: cf.column.clone(),
            from: pf.default.clone(),
            to: cf.default.clone(),
        });
    }
    if pf.max_length != cf.max_length {
        out.push(SchemaChange::AlterColumnMaxLength {
            table: table.to_owned(),
            column: cf.column.clone(),
            from: pf.max_length,
            to: cf.max_length,
        });
    }
    if pf.unique != cf.unique {
        out.push(SchemaChange::AlterColumnUnique {
            table: table.to_owned(),
            column: cf.column.clone(),
            unique: cf.unique,
        });
    }
    // primary_key, min, max, fk, auto changes still reach
    // `detect_unsupported_field_changes` and surface as the v0.3.1
    // hard error — ALTER PRIMARY KEY and CHECK manipulation are
    // dialect-fiddly and need a follow-up slice.
}

/// Detect column metadata changes that even v0.4 can't yet represent
/// — primary-key flips, `min`/`max` (CHECK) changes, FK target
/// changes, `Auto<T>` add/remove. v0.4 added concrete `AlterColumn*`
/// variants for type/nullable/default/max_length, so those are now
/// handled by `detect_changes` and don't surface here. The remaining
/// items still warrant a clear hard-error pointing at a future slice.
///
/// Returns one human-readable diff line per detected change. Empty on
/// success. `make_migrations_from` rejects any non-empty result —
/// otherwise these changes would silently no-op (the field still
/// exists so `detect_changes` skips it; the metadata diff is invisible
/// without explicit ops).
#[must_use]
pub fn detect_unsupported_field_changes(
    prev: &SchemaSnapshot,
    current: &SchemaSnapshot,
) -> Vec<String> {
    let mut out = Vec::new();
    for ct in &current.tables {
        let Some(pt) = prev.table(&ct.name) else {
            continue;
        };
        for cf in &ct.fields {
            let Some(pf) = pt.field(&cf.column) else {
                continue;
            };
            push_field_diffs(&ct.name, pf, cf, &mut out);
        }
    }
    out
}

fn push_field_diffs(table: &str, pf: &FieldSnapshot, cf: &FieldSnapshot, out: &mut Vec<String>) {
    let col = &cf.column;
    // type / nullable / default / max_length are handled by
    // `detect_changes` as `AlterColumn*` ops in v0.4 — don't
    // re-surface them here. The remaining items still need a
    // dedicated slice (PK alters, CHECK alters, FK alters, Auto
    // wrap/unwrap on existing columns).
    if pf.primary_key != cf.primary_key {
        out.push(format!(
            "`{table}.{col}` primary_key changed: {} → {}",
            pf.primary_key, cf.primary_key
        ));
    }
    if pf.min != cf.min {
        out.push(format!(
            "`{table}.{col}` min changed: {:?} → {:?}",
            pf.min, cf.min
        ));
    }
    if pf.max != cf.max {
        out.push(format!(
            "`{table}.{col}` max changed: {:?} → {:?}",
            pf.max, cf.max
        ));
    }
    if pf.fk != cf.fk {
        out.push(format!(
            "`{table}.{col}` fk changed: {:?} → {:?}",
            pf.fk, cf.fk
        ));
    }
    if pf.auto != cf.auto {
        out.push(format!(
            "`{table}.{col}` auto changed: {} → {}",
            pf.auto, cf.auto
        ));
    }
    // `unique` changes are handled by `detect_changes` as
    // `AlterColumnUnique` ops — not surfaced here.
}

/// Render a list of [`SchemaChange`]s as Postgres DDL strings ready to
/// execute. The `current` snapshot is consulted to read field metadata
/// for each `AddColumn` and `CreateTable` (so we know type, nullability,
/// bounds, defaults, etc.).
///
/// **Order is preserved** — this function is order-preserving: changes
/// come out in the same order they came in, with the single exception
/// that FK constraint ALTERs for new tables are appended at the end (so
/// they run after every CREATE TABLE in the batch). Callers that care
/// about dependency-safe ordering (CREATE before ADD COLUMN, DROP COLUMN
/// before DROP TABLE) should hand the changes in already in that order.
/// [`detect_changes`] does that by construction.
///
/// # Errors
/// Returns an error string describing any unsupported change shape (e.g.
/// `AddColumn` referring to a missing field — shouldn't happen if the
/// snapshot was produced by `from_registry`, but worth surfacing).
pub fn render_changes(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
) -> Result<Vec<String>, String> {
    let RenderedBatch {
        mut immediate,
        deferred_fks,
        warnings: _,
    } = render_changes_split(changes, current)?;
    immediate.extend(deferred_fks);
    Ok(immediate)
}

/// DDL rendered for one batch of [`SchemaChange`]s, with FK
/// constraint ALTERs split out from the immediate statements.
///
/// Callers that apply changes one-at-a-time (e.g. the runner walking
/// a `Migration::forward` list interleaved with data ops) need this
/// to defer FK ALTERs until **all** sibling `CreateTable`s in the
/// migration have run — otherwise an early `CreateTable` would emit
/// its FK ALTER referencing a table that hasn't been created yet.
#[derive(Debug, Default)]
pub struct RenderedBatch {
    /// DDL to execute now, in the order it appears here.
    pub immediate: Vec<String>,
    /// FK `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` statements
    /// for new tables in this batch. Run them after every other
    /// migration op has executed so the referenced tables exist.
    pub deferred_fks: Vec<String>,
    /// Non-fatal advisories the writer surfaced during rendering —
    /// e.g. "MySQL has no partial-index syntax; emitted plain UNIQUE
    /// INDEX, add an application-level uniqueness check". Issue #265
    /// / T1.3. Empty when there's nothing to flag.
    pub warnings: Vec<String>,
}

/// Same as [`render_changes`] but keeps FK ALTER constraints in a
/// separate bucket so callers can defer them.
///
/// # Errors
/// As [`render_changes`].
pub fn render_changes_split(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
) -> Result<RenderedBatch, String> {
    // v0.38 — PG-default for back-compat. The migration runner's
    // per-pool apply paths route through `render_changes_split_with_dialect`
    // with the live `&dyn Dialect` so the emitted DDL matches the
    // backend executing it (slice 30 fix — bootstrap CREATE TABLE was
    // emitting `BIGSERIAL` / `TIMESTAMPTZ` on SQLite, which SQLite
    // accepts as NUMERIC affinity columns but then rejects NULL
    // inserts because `BIGSERIAL` doesn't carry the SQLite-specific
    // `INTEGER PRIMARY KEY AUTOINCREMENT` semantics).
    render_changes_split_with_dialect(changes, current, &crate::sql::Postgres)
}

/// v0.38 — dialect-aware counterpart of [`render_changes_split`].
/// The runner's `apply_atomic_pool` / `apply_nonatomic_pool` match
/// arms pass `pool.dialect()` so a SQLite bootstrap migration emits
/// `INTEGER PRIMARY KEY AUTOINCREMENT` for `Auto<i64>` PKs (was
/// `BIGSERIAL`, which SQLite typed as NUMERIC and then rejected
/// NULL inserts into).
///
/// # Errors
/// As [`render_changes_split`].
pub fn render_changes_split_with_dialect(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Result<RenderedBatch, String> {
    render_changes_split_inner(changes, current, dialect)
}

/// Reject `AlterColumn*` operations on dialects whose DDL we
/// don't yet render natively.
///
/// History: the `AlterColumnType / Nullable / Default / MaxLength
/// / Unique` arms emit hand-rolled Postgres syntax — `ALTER TABLE
/// "<t>" ALTER COLUMN "<c>" TYPE / SET NOT NULL / DROP DEFAULT /
/// ...`. Before this guard those arms ignored the `dialect`
/// parameter and emitted PG SQL on MySQL / SQLite, which then
/// failed at apply time with a cryptic database error (MySQL
/// rejects `ALTER COLUMN` — wants `MODIFY COLUMN`; SQLite has no
/// `ALTER COLUMN` at all, only `ALTER TABLE … RENAME COLUMN`).
///
/// Until we ship native MySQL `MODIFY COLUMN` + SQLite
/// table-rebuild rendering (tracked in #559), surface the gap
/// loudly at preview / apply time so operators can swap to a
/// manual `RunSQL` operation before hitting the wall in prod. The
/// PG path is unchanged.
fn guard_alter_column_dialect(
    dialect: &dyn crate::sql::Dialect,
    op: &'static str,
    table: &str,
    column: &str,
) -> Result<(), String> {
    if dialect.name() == "postgres" {
        return Ok(());
    }
    Err(format!(
        "{op} for `{table}.{column}` is not yet supported on dialect `{dialect_name}`. \
         The arm currently emits Postgres-specific DDL (ALTER COLUMN ... TYPE / SET NOT NULL / \
         SET DEFAULT / ADD CONSTRAINT UNIQUE) which would fail at apply time. \
         Workaround: emit a hand-written `Operation::Data` (RunSQL) with the dialect-correct \
         DDL for your migration. Tracked in #559 (per-dialect ALTER COLUMN rendering).",
        op = op,
        table = table,
        column = column,
        dialect_name = dialect.name(),
    ))
}

fn render_changes_split_inner(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Result<RenderedBatch, String> {
    let mut out = RenderedBatch::default();
    for change in changes {
        match change {
            SchemaChange::CreateTable(name) => {
                let table = current.table(name).ok_or_else(|| {
                    format!("CreateTable for `{name}` but no snapshot entry for it")
                })?;
                out.immediate
                    .push(create_table_sql_from_snapshot_with_dialect(table, dialect));
                if !dialect.inline_fks_in_create_table() {
                    out.deferred_fks
                        .extend(constraints_sql_from_snapshot(table, dialect));
                }
            }
            SchemaChange::DropColumn { table, column } => {
                out.immediate.push(format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    dialect.quote_ident(table),
                    dialect.quote_ident(column),
                ));
            }
            SchemaChange::AddColumn { table, column } => {
                let t = current.table(table).ok_or_else(|| {
                    format!("AddColumn for `{table}.{column}` but table missing in snapshot")
                })?;
                let f = t.field(column).ok_or_else(|| {
                    format!("AddColumn for `{table}.{column}` but field missing in snapshot")
                })?;
                if !f.nullable && f.default.is_none() {
                    return Err(format!(
                        "AddColumn `{table}.{column}` is NOT NULL with no `default` — \
                         Postgres can't backfill existing rows. Pick one:\n  \
                         (1) Make the field `Option<…>` — column becomes nullable and existing \
                         rows get NULL.\n  \
                         (2) Set `#[rustango(default = \"…\")]` so existing rows get the \
                         default backfill.\n  \
                         (3) (dev iteration / fresh table only) Delete the pending migration \
                         JSON that emitted this `AddColumn`, then re-run `makemigrations` so \
                         `{column}` lands in the original `CreateTable` for `{table}` — \
                         see #84 in the backlog for the full `migrate --squash` proposal.\n  \
                         Note: option (3) requires the column to NOT exist in the database \
                         yet (i.e. the `CreateTable` migration hasn't been applied, OR you're \
                         willing to drop and recreate the table). Option (1) or (2) is the \
                         right fix for any table that has production data.",
                    ));
                }
                out.immediate.push(add_column_sql(table, f, dialect));
            }
            SchemaChange::DropTable(name) => {
                // CASCADE is Postgres-only — MySQL's parser rejects the
                // keyword and SQLite has no equivalent. (Mirrors the gate in
                // `ddl::drop_table_sql_with_dialect` / `drop_all_pool`.)
                let cascade = if dialect.name() == "postgres" {
                    " CASCADE"
                } else {
                    ""
                };
                out.immediate
                    .push(format!("DROP TABLE {}{cascade}", dialect.quote_ident(name)));
            }
            SchemaChange::AlterColumnType {
                table,
                column,
                from: _,
                to,
            } => {
                guard_alter_column_dialect(dialect, "AlterColumnType", table, column)?;
                let pg_to = pg_type_for_ty_name(to);
                out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" TYPE {pg_to} USING "{column}"::{pg_to}"#,
                ));
            }
            SchemaChange::AlterColumnNullable {
                table,
                column,
                nullable,
            } => {
                guard_alter_column_dialect(dialect, "AlterColumnNullable", table, column)?;
                let action = if *nullable {
                    "DROP NOT NULL"
                } else {
                    "SET NOT NULL"
                };
                out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" {action}"#,
                ));
            }
            SchemaChange::AlterColumnDefault {
                table,
                column,
                from: _,
                to,
            } => {
                guard_alter_column_dialect(dialect, "AlterColumnDefault", table, column)?;
                match to {
                    Some(expr) => out.immediate.push(format!(
                        r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" SET DEFAULT {expr}"#,
                    )),
                    None => out.immediate.push(format!(
                        r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" DROP DEFAULT"#,
                    )),
                }
            }
            SchemaChange::AlterColumnMaxLength {
                table,
                column,
                from: _,
                to,
            } => {
                guard_alter_column_dialect(dialect, "AlterColumnMaxLength", table, column)?;
                let pg_to = match to {
                    Some(n) => format!("VARCHAR({n})"),
                    None => "TEXT".into(),
                };
                out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" TYPE {pg_to} USING "{column}"::{pg_to}"#,
                ));
            }
            SchemaChange::AlterColumnUnique {
                table,
                column,
                unique,
            } => {
                guard_alter_column_dialect(dialect, "AlterColumnUnique", table, column)?;
                if *unique {
                    out.immediate.push(format!(
                        r#"ALTER TABLE "{table}" ADD CONSTRAINT "{table}_{column}_key" UNIQUE ("{column}")"#,
                    ));
                } else {
                    out.immediate.push(format!(
                        r#"ALTER TABLE "{table}" DROP CONSTRAINT "{table}_{column}_key""#,
                    ));
                }
            }
            SchemaChange::RenameTable { old_name, new_name } => {
                out.immediate.push(format!(
                    r#"ALTER TABLE "{old_name}" RENAME TO "{new_name}""#,
                ));
            }
            SchemaChange::RenameColumn {
                table,
                old_column,
                new_column,
            } => {
                out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" RENAME COLUMN "{old_column}" TO "{new_column}""#,
                ));
            }
            SchemaChange::CreateIndex {
                name,
                table,
                columns,
                unique,
                method,
                where_clause,
                include,
            } => {
                let unique_kw = if *unique { "UNIQUE " } else { "" };
                let if_not_exists = if dialect.supports_create_index_if_not_exists() {
                    "IF NOT EXISTS "
                } else {
                    ""
                };
                let cols = columns
                    .iter()
                    .map(|c| dialect.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                // `USING <method>` — emit only when non-default and
                // the dialect honours the keyword. Backends that
                // don't support the method silently fall through to
                // their default (btree); see `IndexMethod` docs.
                let using = dialect.index_method_clause(method);
                // Partial-index `WHERE <expr>` — issue #265 / T1.3.
                // PG / SQLite ship native partial indexes; MySQL has
                // no equivalent (the writer drops the clause + emits
                // a warning so the rest of the migration still
                // applies). The dialect controls inclusion.
                let where_suffix = if let Some(expr) = where_clause {
                    if dialect.supports_partial_index() {
                        format!(" WHERE {expr}")
                    } else {
                        out.warnings.push(format!(
                            "index {name:?} declares a partial WHERE \
                             clause ({expr:?}); {} has no partial-index \
                             syntax — emitting plain UNIQUE INDEX. Add \
                             an application-level uniqueness check to \
                             cover the partition.",
                            dialect.name()
                        ));
                        String::new()
                    }
                } else {
                    String::new()
                };
                // Covering-index `INCLUDE (cols)` — PG 11+ ships it.
                // SQLite + MySQL lack it; the writer drops the clause
                // with a warning so the rest of the migration still
                // applies. Routes through the dialect via the new
                // `supports("covering_index")` capability token —
                // Postgres's `supports` override advertises it under
                // the same name (`expression_index`-adjacent), but
                // PG-specific. The simplest gate is the existing
                // `dialect.name() == "postgres"` since covering
                // indexes are PG-only across rustango's matrix.
                let include_suffix = if include.is_empty() {
                    String::new()
                } else if dialect.name() == "postgres" {
                    let cols = include
                        .iter()
                        .map(|c| dialect.quote_ident(c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(" INCLUDE ({cols})")
                } else {
                    out.warnings.push(format!(
                        "index {name:?} declares INCLUDE ({}); {} has \
                         no covering-index syntax — emitting plain \
                         index. Add a redundant non-key column to the \
                         key tuple if you need the cover.",
                        include.join(", "),
                        dialect.name()
                    ));
                    String::new()
                };
                out.immediate.push(format!(
                    "CREATE {unique_kw}INDEX {if_not_exists}{} ON {}{} ({cols}){include_suffix}{where_suffix}",
                    dialect.quote_ident(name),
                    dialect.quote_ident(table),
                    using,
                ));
            }
            SchemaChange::DropIndex { name } => {
                // MySQL needs `DROP INDEX <name> ON <table>` and rejects
                // `IF EXISTS`. The variant doesn't carry the table — fail
                // loudly until #559 promotes `DropIndex` to include it.
                if dialect.name() == "mysql" {
                    return Err(format!(
                        "DropIndex for `{name}` is not yet supported on dialect `mysql`. \
                         MySQL requires the table name (`DROP INDEX <name> ON <table>`) but \
                         the `SchemaChange::DropIndex` variant only carries `name`. \
                         Workaround: emit a hand-written `Operation::Data` (RunSQL) with the \
                         dialect-correct DDL for your migration. Tracked in #559."
                    ));
                }
                out.immediate.push(format!(
                    "DROP INDEX IF EXISTS {}",
                    dialect.quote_ident(name)
                ));
            }
            SchemaChange::AddCheckConstraint { name, table, expr } => {
                if dialect.name() == "sqlite" {
                    return Err(format!(
                        "AddCheckConstraint for `{table}.{name}` is not yet supported on \
                         dialect `sqlite`. SQLite has no `ALTER TABLE ADD CONSTRAINT CHECK` \
                         syntax — CHECK constraints must be declared inside the original \
                         CREATE TABLE statement. Workaround: emit a hand-written \
                         `Operation::Data` (RunSQL) that rebuilds the table with the CHECK \
                         inline, or use an application-level invariant. Tracked in #559."
                    ));
                }
                out.immediate.push(format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({expr})",
                    dialect.quote_ident(table),
                    dialect.quote_ident(name),
                ));
            }
            SchemaChange::DropCheckConstraint { name, table } => {
                if dialect.name() == "sqlite" {
                    return Err(format!(
                        "DropCheckConstraint for `{table}.{name}` is not yet supported on \
                         dialect `sqlite`. SQLite has no `ALTER TABLE DROP CONSTRAINT` \
                         syntax. Workaround: emit a hand-written `Operation::Data` (RunSQL) \
                         that rebuilds the table without the CHECK. Tracked in #559."
                    ));
                }
                out.immediate.push(format!(
                    "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
                    dialect.quote_ident(table),
                    dialect.quote_ident(name),
                ));
            }
            SchemaChange::AddExclusionConstraint {
                name,
                table,
                using,
                elements,
                where_clause,
            } => {
                if dialect.name() != "postgres" {
                    // MySQL + SQLite have no EXCLUDE constraint. Emit
                    // nothing and warn so the rest of the migration
                    // applies cleanly; the user's app is responsible
                    // for porting the constraint to an
                    // application-level check (transaction-scoped
                    // SELECT + UPDATE) on these backends. Issue #32.
                    tracing::warn!(
                        constraint = %name,
                        table = %table,
                        dialect = dialect.name(),
                        "skipping AddExclusionConstraint — PG-only, no equivalent on this backend",
                    );
                    continue;
                }
                // PG: `ALTER TABLE "t" ADD CONSTRAINT "n" EXCLUDE
                // USING <using> ("col1" WITH op1, "col2" WITH op2)
                // [WHERE (<predicate>)]`.
                let elem_sql: Vec<String> = elements
                    .iter()
                    .map(|(col, op)| format!(r#""{col}" WITH {op}"#))
                    .collect();
                let mut stmt = format!(
                    r#"ALTER TABLE "{table}" ADD CONSTRAINT "{name}" EXCLUDE USING {using} ({})"#,
                    elem_sql.join(", "),
                );
                if let Some(pred) = where_clause {
                    stmt.push_str(&format!(" WHERE ({pred})"));
                }
                out.immediate.push(stmt);
            }
            SchemaChange::DropExclusionConstraint { name, table } => {
                if dialect.name() != "postgres" {
                    tracing::warn!(
                        constraint = %name,
                        table = %table,
                        dialect = dialect.name(),
                        "skipping DropExclusionConstraint — PG-only",
                    );
                    continue;
                }
                out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" DROP CONSTRAINT IF EXISTS "{name}""#,
                ));
            }
            SchemaChange::CreateM2MTable {
                through,
                src_table,
                src_col,
                dst_table,
                dst_col,
            } => {
                let q_through = dialect.quote_ident(through);
                let q_src_col = dialect.quote_ident(src_col);
                let q_dst_col = dialect.quote_ident(dst_col);
                let q_src_table = dialect.quote_ident(src_table);
                let q_dst_table = dialect.quote_ident(dst_table);
                let q_id = dialect.quote_ident("id");
                let q_src_fk = dialect.quote_ident(&format!("{through}_{src_col}_fkey"));
                let q_dst_fk = dialect.quote_ident(&format!("{through}_{dst_col}_fkey"));

                if dialect.inline_fks_in_create_table() {
                    // SQLite: ALTER TABLE … ADD CONSTRAINT FK isn't supported.
                    // Emit the FK clauses inside the CREATE TABLE statement.
                    out.immediate.push(format!(
                        "CREATE TABLE {q_through} ({q_src_col} BIGINT NOT NULL, {q_dst_col} BIGINT NOT NULL, \
                         PRIMARY KEY ({q_src_col}, {q_dst_col}), \
                         CONSTRAINT {q_src_fk} FOREIGN KEY ({q_src_col}) REFERENCES {q_src_table} ({q_id}) ON DELETE CASCADE, \
                         CONSTRAINT {q_dst_fk} FOREIGN KEY ({q_dst_col}) REFERENCES {q_dst_table} ({q_id}) ON DELETE CASCADE)",
                    ));
                } else {
                    // PG / MySQL: defer FK creation so cross-table cycles
                    // resolve cleanly within a single migration batch.
                    out.immediate.push(format!(
                        "CREATE TABLE {q_through} ({q_src_col} BIGINT NOT NULL, {q_dst_col} BIGINT NOT NULL, \
                         PRIMARY KEY ({q_src_col}, {q_dst_col}))",
                    ));
                    out.deferred_fks.push(format!(
                        "ALTER TABLE {q_through} ADD CONSTRAINT {q_src_fk} \
                         FOREIGN KEY ({q_src_col}) REFERENCES {q_src_table} ({q_id}) ON DELETE CASCADE",
                    ));
                    out.deferred_fks.push(format!(
                        "ALTER TABLE {q_through} ADD CONSTRAINT {q_dst_fk} \
                         FOREIGN KEY ({q_dst_col}) REFERENCES {q_dst_table} ({q_id}) ON DELETE CASCADE",
                    ));
                }
            }
            SchemaChange::DropM2MTable { through } => {
                let cascade = if dialect.name() == "postgres" {
                    " CASCADE"
                } else {
                    ""
                };
                out.immediate.push(format!(
                    "DROP TABLE IF EXISTS {}{cascade}",
                    dialect.quote_ident(through)
                ));
            }
            SchemaChange::AddCompositeFk {
                table,
                name,
                to,
                from,
                on,
            } => {
                if dialect.name() == "sqlite" {
                    return Err(format!(
                        "AddCompositeFk for `{table}.{name}` is not yet supported on \
                         dialect `sqlite`. SQLite has no `ALTER TABLE ADD CONSTRAINT FOREIGN \
                         KEY` syntax — composite FKs must be declared inside the original \
                         CREATE TABLE statement. Workaround: emit a hand-written \
                         `Operation::Data` (RunSQL) that rebuilds the table with the FK \
                         inline. Tracked in #559."
                    ));
                }
                let from_cols = from
                    .iter()
                    .map(|c| dialect.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let on_cols = on
                    .iter()
                    .map(|c| dialect.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.deferred_fks.push(format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({from_cols}) REFERENCES {} ({on_cols})",
                    dialect.quote_ident(table),
                    dialect.quote_ident(name),
                    dialect.quote_ident(to),
                ));
            }
            SchemaChange::DropCompositeFk { table, name } => {
                if dialect.name() == "sqlite" {
                    return Err(format!(
                        "DropCompositeFk for `{table}.{name}` is not yet supported on \
                         dialect `sqlite`. SQLite has no `ALTER TABLE DROP CONSTRAINT` \
                         syntax. Workaround: emit a hand-written `Operation::Data` (RunSQL) \
                         that rebuilds the table without the FK. Tracked in #559."
                    ));
                }
                out.immediate.push(format!(
                    "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
                    dialect.quote_ident(table),
                    dialect.quote_ident(name),
                ));
            }
        }
    }
    Ok(out)
}

/// Map a `FieldSnapshot.ty` name (matches `FieldType::as_str` in
/// rustango-core, but kept loose here for forward-compat with future
/// types externally-supplied migration files might carry) to its
/// Postgres column type. Used by `AlterColumnType`. For String,
/// returns `TEXT` — `AlterColumnMaxLength` is the dedicated
/// `VARCHAR(N)` rename op.
fn pg_type_for_ty_name(ty: &str) -> String {
    match ty {
        "i16" => "SMALLINT".into(),
        "i32" => "INTEGER".into(),
        "i64" => "BIGINT".into(),
        "f32" => "REAL".into(),
        "f64" => "DOUBLE PRECISION".into(),
        "bool" => "BOOLEAN".into(),
        "string" => "TEXT".into(),
        "datetime" => "TIMESTAMPTZ".into(),
        "date" => "DATE".into(),
        "time" => "TIME".into(),
        "uuid" => "UUID".into(),
        "json" => "JSONB".into(),
        "decimal" => "NUMERIC".into(),
        "binary" => "BYTEA".into(),
        // #341 — PG array element kinds.
        "array_text" => "text[]".into(),
        "array_int" => "integer[]".into(),
        "array_bigint" => "bigint[]".into(),
        // #343 — PG range element kinds.
        "range_int" => "int4range".into(),
        "range_bigint" => "int8range".into(),
        "range_numeric" => "numrange".into(),
        "range_date" => "daterange".into(),
        "range_datetime" => "tstzrange".into(),
        other => other.to_uppercase(),
    }
}

fn create_table_sql_from_snapshot_with_dialect(
    t: &TableSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> String {
    let mut sql = format!("CREATE TABLE {} (", dialect.quote_ident(&t.name));
    let mut first = true;
    for f in &t.fields {
        if !first {
            sql.push_str(", ");
        }
        first = false;
        let _ = write!(
            sql,
            "{} {}",
            dialect.quote_ident(&f.column),
            sql_type_with_dialect(f, dialect)
        );
        // Generated columns (#559) — `GENERATED ALWAYS AS (<expr>) STORED`.
        // Skips DEFAULT / PRIMARY KEY / UNIQUE / CHECK (Postgres rejects
        // all of these on generated columns). NOT NULL stays permitted.
        // Mirrors the live-registry behavior in
        // `migrate::ddl::write_column_def`.
        if let Some(expr) = &f.generated_as {
            let _ = write!(sql, " GENERATED ALWAYS AS ({expr}) STORED");
            if !f.nullable {
                sql.push_str(" NOT NULL");
            }
            // db_comment still applies on MySQL even for generated columns.
            if let Some(comment) = &f.db_comment {
                if let Some(inline) = dialect.write_inline_column_comment(comment) {
                    sql.push_str(&inline);
                }
            }
            continue;
        }
        if let Some(expr) = &f.default {
            let _ = write!(
                sql,
                " DEFAULT {}",
                dialect.translate_default_expr(expr, &f.ty, f.max_length)
            );
        }
        if !f.nullable {
            sql.push_str(" NOT NULL");
        }
        // SQLite emits `INTEGER PRIMARY KEY AUTOINCREMENT` as a single
        // type token for `Auto<T>` PKs — `PRIMARY KEY` is part of the
        // type, not a separate clause. Skip the standalone append.
        let serial_pk_inline = f.auto
            && matches!(f.ty.as_str(), "i16" | "i32" | "i64")
            && dialect.serial_type_includes_primary_key();
        if f.primary_key && !serial_pk_inline {
            sql.push_str(" PRIMARY KEY");
        }
        // Per-column UNIQUE constraint from #[rustango(unique)]. Without
        // this clause the snapshot's `unique: true` flag was honoured by
        // the diff path (AlterColumnUnique) but silently dropped when a
        // CreateTable rendered the initial DDL — surfaced live by the
        // cookbook /authors/new playwright session, where two Authors
        // with the same email INSERT-ed cleanly despite the model
        // declaring `#[rustango(unique)]`.
        if f.unique && !f.primary_key {
            sql.push_str(" UNIQUE");
        }
        if f.min.is_some() || f.max.is_some() {
            sql.push_str(" CHECK (");
            let mut wrote = false;
            if let Some(min) = f.min {
                let _ = write!(sql, "{} >= {}", dialect.quote_ident(&f.column), min);
                wrote = true;
            }
            if let Some(max) = f.max {
                if wrote {
                    sql.push_str(" AND ");
                }
                let _ = write!(sql, "{} <= {}", dialect.quote_ident(&f.column), max);
            }
            sql.push(')');
        }
        // db_comment (#559) — MySQL inlines `COMMENT '...'` on the column
        // line. PG gets a separate `COMMENT ON COLUMN` statement post-
        // CREATE; SQLite has no native column comments.
        if let Some(comment) = &f.db_comment {
            if let Some(inline) = dialect.write_inline_column_comment(comment) {
                sql.push_str(&inline);
            }
        }
        // Inline FK clause for dialects that can't ALTER TABLE ADD CONSTRAINT
        // (SQLite). Postgres/MySQL keep the post-hoc ALTER path so cyclic
        // FK graphs resolve across the whole migration batch.
        if dialect.inline_fks_in_create_table() {
            if let Some(rel) = &f.fk {
                let _ = write!(
                    sql,
                    " REFERENCES {} ({})",
                    dialect.quote_ident(&rel.to),
                    dialect.quote_ident(&rel.on),
                );
            }
        }
    }
    if dialect.inline_fks_in_create_table() {
        for cf in &t.composite_fks {
            sql.push_str(", FOREIGN KEY (");
            let from_cols: Vec<String> = cf.from.iter().map(|c| dialect.quote_ident(c)).collect();
            sql.push_str(&from_cols.join(", "));
            sql.push_str(") REFERENCES ");
            sql.push_str(&dialect.quote_ident(&cf.to));
            sql.push_str(" (");
            let on_cols: Vec<String> = cf.on.iter().map(|c| dialect.quote_ident(c)).collect();
            sql.push_str(&on_cols.join(", "));
            sql.push(')');
        }
    }
    sql.push(')');
    sql
}

fn constraints_sql_from_snapshot(
    t: &TableSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Vec<String> {
    let table_q = dialect.quote_ident(&t.name);
    let mut out: Vec<String> = t
        .fields
        .iter()
        .filter_map(|f| {
            f.fk.as_ref().map(|rel| {
                let constraint = format!("{}_{}_fkey", t.name, f.column);
                format!(
                    "ALTER TABLE {table_q} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
                    dialect.quote_ident(&constraint),
                    dialect.quote_ident(&f.column),
                    dialect.quote_ident(&rel.to),
                    dialect.quote_ident(&rel.on),
                )
            })
        })
        .collect();
    for cf in &t.composite_fks {
        let from_cols: Vec<String> = cf.from.iter().map(|c| dialect.quote_ident(c)).collect();
        let on_cols: Vec<String> = cf.on.iter().map(|c| dialect.quote_ident(c)).collect();
        out.push(format!(
            "ALTER TABLE {table_q} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
            dialect.quote_ident(&cf.name),
            from_cols.join(", "),
            dialect.quote_ident(&cf.to),
            on_cols.join(", "),
        ));
    }
    out
}

fn add_column_sql(table: &str, f: &FieldSnapshot, dialect: &dyn crate::sql::Dialect) -> String {
    let col_q = dialect.quote_ident(&f.column);
    let mut sql = format!(
        "ALTER TABLE {} ADD COLUMN {} {}",
        dialect.quote_ident(table),
        col_q,
        sql_type_with_dialect(f, dialect)
    );
    if let Some(expr) = &f.default {
        let _ = write!(
            sql,
            " DEFAULT {}",
            dialect.translate_default_expr(expr, &f.ty, f.max_length)
        );
    }
    if !f.nullable {
        sql.push_str(" NOT NULL");
    }
    if f.min.is_some() || f.max.is_some() {
        sql.push_str(" CHECK (");
        let mut wrote = false;
        if let Some(min) = f.min {
            let _ = write!(sql, "{col_q} >= {min}");
            wrote = true;
        }
        if let Some(max) = f.max {
            if wrote {
                sql.push_str(" AND ");
            }
            let _ = write!(sql, "{col_q} <= {max}");
        }
        sql.push(')');
    }
    sql
}

#[cfg(test)]
fn sql_type(f: &FieldSnapshot) -> String {
    // v0.38 — PG-default kept for tests. Tri-dialect emitters go
    // through `sql_type_with_dialect`.
    sql_type_with_dialect(f, &crate::sql::Postgres)
}

/// v0.38 — dialect-aware sql_type. Routes integer-`Auto<T>` PKs
/// through `dialect.serial_type()` and every other field through
/// `dialect.column_type()`. SQLite's `Auto<i64>` PK becomes
/// `INTEGER PRIMARY KEY AUTOINCREMENT`; MySQL's becomes `BIGINT NOT NULL
/// AUTO_INCREMENT`; PG's stays `BIGSERIAL`.
fn sql_type_with_dialect(f: &FieldSnapshot, dialect: &dyn crate::sql::Dialect) -> String {
    use crate::core::FieldType;
    // Map snapshot's lowercase ty string to FieldType.
    let ty = match f.ty.as_str() {
        "i16" => Some(FieldType::I16),
        "i32" => Some(FieldType::I32),
        "i64" => Some(FieldType::I64),
        "f32" => Some(FieldType::F32),
        "f64" => Some(FieldType::F64),
        "bool" => Some(FieldType::Bool),
        "string" => Some(FieldType::String),
        "datetime" => Some(FieldType::DateTime),
        "date" => Some(FieldType::Date),
        "time" => Some(FieldType::Time),
        "uuid" => Some(FieldType::Uuid),
        "json" => Some(FieldType::Json),
        "decimal" => Some(FieldType::Decimal),
        "binary" => Some(FieldType::Binary),
        // #341 — PG array element kinds; route through `column_type`
        // (→ `text[]` / `integer[]` / `bigint[]` on PG).
        "array_text" => Some(FieldType::Array(crate::core::ArrayElem::Text)),
        "array_int" => Some(FieldType::Array(crate::core::ArrayElem::Int)),
        "array_bigint" => Some(FieldType::Array(crate::core::ArrayElem::BigInt)),
        // #343 — PG range element kinds.
        "range_int" => Some(FieldType::Range(crate::core::RangeElem::Int)),
        "range_bigint" => Some(FieldType::Range(crate::core::RangeElem::BigInt)),
        "range_numeric" => Some(FieldType::Range(crate::core::RangeElem::Numeric)),
        "range_date" => Some(FieldType::Range(crate::core::RangeElem::Date)),
        "range_datetime" => Some(FieldType::Range(crate::core::RangeElem::DateTime)),
        _ => None,
    };
    // v0.13.2: `auto = true` historically meant "PK SERIAL/BIGSERIAL"
    // for integer types; field-mixin auto (auto_now_add etc.) on
    // non-integer types falls through to the regular column_type
    // mapping. Dialect-specific token is picked by `dialect.serial_type()`.
    if f.auto {
        if let Some(t) = ty {
            if matches!(t, FieldType::I16 | FieldType::I32 | FieldType::I64) {
                return dialect.serial_type(t).to_owned();
            }
        }
    }
    // #344 — case-insensitive String columns route through
    // `dialect.ci_text_type` (PG → CITEXT, SQLite → TEXT COLLATE
    // NOCASE, MySQL → TEXT COLLATE utf8mb4_general_ci).
    if f.case_insensitive {
        if matches!(ty, Some(FieldType::String)) {
            return dialect.ci_text_type(f.max_length);
        }
    }
    if let Some(t) = ty {
        return dialect.column_type(t, f.max_length);
    }
    // Unknown type string — fall through to upper-case (preserves
    // pre-v0.38 behavior for any field type rustango doesn't model).
    f.ty.to_uppercase()
}

#[cfg(test)]
mod sql_type_tests {
    use super::*;
    use crate::migrate::snapshot::FieldSnapshot;

    fn fs(ty: &str, auto: bool) -> FieldSnapshot {
        FieldSnapshot {
            name: "x".into(),
            column: "x".into(),
            ty: ty.into(),
            nullable: false,
            primary_key: false,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto,
            unique: false,
            case_insensitive: false,
            generated_as: None,
            db_comment: None,
            fk: None,
        }
    }

    #[test]
    fn auto_integer_emits_serial() {
        assert_eq!(sql_type(&fs("i32", true)), "SERIAL");
        assert_eq!(sql_type(&fs("i64", true)), "BIGSERIAL");
    }

    #[test]
    fn auto_non_integer_falls_through_to_real_type() {
        // v0.13.2 — B1 from the rustail postmortem. Auto on
        // non-integer types (auto_now_add / auto_now / auto_uuid)
        // must emit the real Postgres column type, not an
        // upper-cased version of the rustango-internal name. A
        // CREATE TABLE with `"created_at" DATETIME ...` makes
        // Postgres reject the migration.
        assert_eq!(sql_type(&fs("datetime", true)), "TIMESTAMPTZ");
        assert_eq!(sql_type(&fs("date", true)), "DATE");
        assert_eq!(sql_type(&fs("uuid", true)), "UUID");
        assert_eq!(sql_type(&fs("bool", true)), "BOOLEAN");
        assert_eq!(sql_type(&fs("string", true)), "TEXT");
    }

    #[test]
    fn non_auto_passes_through_normally() {
        assert_eq!(sql_type(&fs("i64", false)), "BIGINT");
        assert_eq!(sql_type(&fs("datetime", false)), "TIMESTAMPTZ");
    }

    // #559 — DROP arms must be dialect-aware. `CASCADE` is Postgres-only
    // (MySQL's parser rejects it, SQLite has no equivalent) and identifier
    // quoting differs (PG/SQLite double-quote, MySQL backtick).
    #[test]
    fn drop_arms_are_dialect_aware() {
        use crate::migrate::{SchemaChange, SchemaSnapshot};
        let snap = SchemaSnapshot::from_models(&[]);

        let drop_table = [SchemaChange::DropTable("foo".into())];
        let pg =
            render_changes_split_with_dialect(&drop_table, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(
            pg.immediate,
            vec![r#"DROP TABLE "foo" CASCADE"#.to_string()]
        );
        #[cfg(feature = "mysql")]
        {
            let my =
                render_changes_split_with_dialect(&drop_table, &snap, &crate::sql::MySql).unwrap();
            assert_eq!(my.immediate, vec!["DROP TABLE `foo`".to_string()]);
        }
        #[cfg(feature = "sqlite")]
        {
            let sq =
                render_changes_split_with_dialect(&drop_table, &snap, &crate::sql::Sqlite).unwrap();
            assert_eq!(sq.immediate, vec![r#"DROP TABLE "foo""#.to_string()]);
        }

        let drop_col = [SchemaChange::DropColumn {
            table: "t".into(),
            column: "c".into(),
        }];
        #[cfg(feature = "mysql")]
        {
            let my =
                render_changes_split_with_dialect(&drop_col, &snap, &crate::sql::MySql).unwrap();
            assert_eq!(
                my.immediate,
                vec!["ALTER TABLE `t` DROP COLUMN `c`".to_string()]
            );
        }
    }

    // -------- guard_alter_column_dialect (#559 protection) --------

    fn empty_snap() -> SchemaSnapshot {
        SchemaSnapshot::default()
    }

    #[test]
    fn alter_column_type_emits_pg_sql_on_postgres() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnType {
            table: "t".into(),
            column: "c".into(),
            from: "i32".into(),
            to: "i64".into(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(out.immediate.len(), 1);
        assert!(out.immediate[0].contains("ALTER COLUMN \"c\" TYPE"));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn alter_column_type_errors_on_mysql() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnType {
            table: "t".into(),
            column: "c".into(),
            from: "i32".into(),
            to: "i64".into(),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql)
            .expect_err("AlterColumnType must reject MySQL until native rendering ships");
        assert!(err.contains("AlterColumnType"));
        assert!(err.contains("`t.c`"));
        assert!(err.contains("mysql"));
        assert!(err.contains("#559"));
        // Workaround pointer present.
        assert!(err.contains("RunSQL"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn alter_column_nullable_errors_on_sqlite() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnNullable {
            table: "t".into(),
            column: "c".into(),
            nullable: false,
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite)
            .expect_err("AlterColumnNullable must reject SQLite until rebuild path ships");
        assert!(err.contains("AlterColumnNullable"));
        assert!(err.contains("sqlite"));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn alter_column_default_errors_on_mysql() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnDefault {
            table: "t".into(),
            column: "c".into(),
            from: None,
            to: Some("'hi'".into()),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql)
            .expect_err("AlterColumnDefault must reject MySQL");
        assert!(err.contains("AlterColumnDefault"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn alter_column_max_length_errors_on_sqlite() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnMaxLength {
            table: "t".into(),
            column: "c".into(),
            from: Some(50),
            to: Some(100),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite)
            .expect_err("AlterColumnMaxLength must reject SQLite");
        assert!(err.contains("AlterColumnMaxLength"));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn alter_column_unique_errors_on_mysql() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnUnique {
            table: "t".into(),
            column: "c".into(),
            unique: true,
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql)
            .expect_err("AlterColumnUnique must reject MySQL");
        assert!(err.contains("AlterColumnUnique"));
    }

    // -------- CreateM2MTable (#559 tri-dialect) --------

    fn make_create_m2m() -> Vec<SchemaChange> {
        vec![SchemaChange::CreateM2MTable {
            through: "post_tags".into(),
            src_table: "posts".into(),
            src_col: "post_id".into(),
            dst_table: "tags".into(),
            dst_col: "tag_id".into(),
        }]
    }

    #[test]
    fn create_m2m_table_postgres_defers_fk_constraints() {
        let snap = empty_snap();
        let out =
            render_changes_split_with_dialect(&make_create_m2m(), &snap, &crate::sql::Postgres)
                .unwrap();
        assert_eq!(out.immediate.len(), 1);
        // PG: ANSI " quoting + bare CREATE TABLE without FKs inline.
        assert!(out.immediate[0].contains(r#"CREATE TABLE "post_tags""#));
        assert!(out.immediate[0].contains(r#"PRIMARY KEY ("post_id", "tag_id")"#));
        // FKs deferred to ALTER TABLE ADD CONSTRAINT.
        assert_eq!(out.deferred_fks.len(), 2);
        assert!(out.deferred_fks[0].contains(r#"ADD CONSTRAINT "post_tags_post_id_fkey""#));
        assert!(out.deferred_fks[0].contains(r#"REFERENCES "posts" ("id")"#));
        assert!(out.deferred_fks[1].contains(r#"ADD CONSTRAINT "post_tags_tag_id_fkey""#));
        assert!(out.deferred_fks[1].contains(r#"REFERENCES "tags" ("id")"#));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn create_m2m_table_mysql_uses_backtick_quoting() {
        let snap = empty_snap();
        let out = render_changes_split_with_dialect(&make_create_m2m(), &snap, &crate::sql::MySql)
            .unwrap();
        // MySQL must use backticks, NOT ANSI double-quotes.
        assert_eq!(out.immediate.len(), 1);
        assert!(out.immediate[0].contains("CREATE TABLE `post_tags`"));
        assert!(out.immediate[0].contains("PRIMARY KEY (`post_id`, `tag_id`)"));
        assert!(
            !out.immediate[0].contains('"'),
            "MySQL must not contain ANSI double-quotes: {}",
            out.immediate[0]
        );
        assert_eq!(out.deferred_fks.len(), 2);
        assert!(out.deferred_fks[0].contains("ADD CONSTRAINT `post_tags_post_id_fkey`"));
        assert!(out.deferred_fks[0].contains("REFERENCES `posts` (`id`)"));
        for stmt in &out.deferred_fks {
            assert!(!stmt.contains('"'), "MySQL FK must use backticks: {stmt}");
        }
    }

    // -------- DropIndex / AddCheckConstraint / DropCheckConstraint (#559) --------

    #[test]
    fn drop_index_postgres_uses_ansi_quoting_with_if_exists() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropIndex {
            name: "idx_post_slug".into(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(
            out.immediate,
            vec![r#"DROP INDEX IF EXISTS "idx_post_slug""#.to_string()]
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn drop_index_sqlite_uses_ansi_quoting_with_if_exists() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropIndex {
            name: "idx_post_slug".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite).unwrap();
        assert_eq!(
            out.immediate,
            vec![r#"DROP INDEX IF EXISTS "idx_post_slug""#.to_string()]
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn drop_index_mysql_rejects_until_variant_carries_table() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropIndex {
            name: "idx_post_slug".into(),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql)
            .expect_err("DropIndex must reject MySQL — variant lacks table reference");
        assert!(err.contains("DropIndex"));
        assert!(err.contains("idx_post_slug"));
        assert!(err.contains("mysql"));
        assert!(err.contains("ON <table>"));
    }

    #[test]
    fn add_check_constraint_postgres_works() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AddCheckConstraint {
            name: "ck_post_views_nonneg".into(),
            table: "posts".into(),
            expr: "views >= 0".into(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(
            out.immediate,
            vec![
                r#"ALTER TABLE "posts" ADD CONSTRAINT "ck_post_views_nonneg" CHECK (views >= 0)"#
                    .to_string()
            ]
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn add_check_constraint_mysql_uses_backticks() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AddCheckConstraint {
            name: "ck_post_views_nonneg".into(),
            table: "posts".into(),
            expr: "views >= 0".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql).unwrap();
        assert_eq!(
            out.immediate,
            vec![
                "ALTER TABLE `posts` ADD CONSTRAINT `ck_post_views_nonneg` CHECK (views >= 0)"
                    .to_string()
            ]
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn add_check_constraint_sqlite_rejects() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::AddCheckConstraint {
            name: "ck_post_views_nonneg".into(),
            table: "posts".into(),
            expr: "views >= 0".into(),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite)
            .expect_err("AddCheckConstraint must reject SQLite");
        assert!(err.contains("AddCheckConstraint"));
        assert!(err.contains("sqlite"));
        assert!(err.contains("ALTER TABLE ADD CONSTRAINT CHECK"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn drop_check_constraint_sqlite_rejects() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCheckConstraint {
            name: "ck_post_views_nonneg".into(),
            table: "posts".into(),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite)
            .expect_err("DropCheckConstraint must reject SQLite");
        assert!(err.contains("DropCheckConstraint"));
        assert!(err.contains("sqlite"));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn drop_check_constraint_mysql_uses_backticks() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCheckConstraint {
            name: "ck_x".into(),
            table: "t".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql).unwrap();
        assert_eq!(
            out.immediate,
            vec!["ALTER TABLE `t` DROP CONSTRAINT IF EXISTS `ck_x`".to_string()]
        );
    }

    // -------- Add/DropCompositeFk (#559) --------

    fn make_add_composite_fk() -> Vec<SchemaChange> {
        vec![SchemaChange::AddCompositeFk {
            table: "child".into(),
            name: "fk_child_parent_composite".into(),
            to: "parent".into(),
            from: vec!["pa_id".into(), "pb_id".into()],
            on: vec!["a_id".into(), "b_id".into()],
        }]
    }

    #[test]
    fn add_composite_fk_postgres_defers_with_ansi_quoting() {
        let snap = empty_snap();
        let out = render_changes_split_with_dialect(
            &make_add_composite_fk(),
            &snap,
            &crate::sql::Postgres,
        )
        .unwrap();
        assert_eq!(out.immediate.len(), 0);
        assert_eq!(out.deferred_fks.len(), 1);
        let stmt = &out.deferred_fks[0];
        assert!(stmt.contains(r#"ALTER TABLE "child""#));
        assert!(stmt.contains(r#"ADD CONSTRAINT "fk_child_parent_composite""#));
        assert!(stmt.contains(r#"FOREIGN KEY ("pa_id", "pb_id")"#));
        assert!(stmt.contains(r#"REFERENCES "parent" ("a_id", "b_id")"#));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn add_composite_fk_mysql_uses_backticks() {
        let snap = empty_snap();
        let out =
            render_changes_split_with_dialect(&make_add_composite_fk(), &snap, &crate::sql::MySql)
                .unwrap();
        assert_eq!(out.deferred_fks.len(), 1);
        let stmt = &out.deferred_fks[0];
        assert!(stmt.contains("ALTER TABLE `child`"));
        assert!(stmt.contains("ADD CONSTRAINT `fk_child_parent_composite`"));
        assert!(stmt.contains("FOREIGN KEY (`pa_id`, `pb_id`)"));
        assert!(stmt.contains("REFERENCES `parent` (`a_id`, `b_id`)"));
        assert!(
            !stmt.contains('"'),
            "MySQL output must not contain ANSI quotes: {stmt}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn add_composite_fk_sqlite_rejects() {
        let err = render_changes_split_with_dialect(
            &make_add_composite_fk(),
            &empty_snap(),
            &crate::sql::Sqlite,
        )
        .expect_err("AddCompositeFk must reject SQLite");
        assert!(err.contains("AddCompositeFk"));
        assert!(err.contains("sqlite"));
        assert!(err.contains("ADD CONSTRAINT FOREIGN KEY"));
    }

    #[test]
    fn drop_composite_fk_postgres_uses_ansi_quoting() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCompositeFk {
            table: "child".into(),
            name: "fk_x".into(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(
            out.immediate,
            vec![r#"ALTER TABLE "child" DROP CONSTRAINT IF EXISTS "fk_x""#.to_string()]
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn drop_composite_fk_mysql_uses_backticks() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCompositeFk {
            table: "child".into(),
            name: "fk_x".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql).unwrap();
        assert_eq!(
            out.immediate,
            vec!["ALTER TABLE `child` DROP CONSTRAINT IF EXISTS `fk_x`".to_string()]
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn drop_composite_fk_sqlite_rejects() {
        let changes = vec![SchemaChange::DropCompositeFk {
            table: "child".into(),
            name: "fk_x".into(),
        }];
        let err = render_changes_split_with_dialect(&changes, &empty_snap(), &crate::sql::Sqlite)
            .expect_err("DropCompositeFk must reject SQLite");
        assert!(err.contains("DropCompositeFk"));
        assert!(err.contains("sqlite"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn create_m2m_table_sqlite_inlines_fks_in_create() {
        let snap = empty_snap();
        let out = render_changes_split_with_dialect(&make_create_m2m(), &snap, &crate::sql::Sqlite)
            .unwrap();
        // SQLite: FKs MUST be inline in CREATE TABLE — no deferred ALTERs.
        assert_eq!(out.immediate.len(), 1);
        assert_eq!(
            out.deferred_fks.len(),
            0,
            "SQLite must NOT defer FK constraints: {:?}",
            out.deferred_fks
        );
        let stmt = &out.immediate[0];
        assert!(stmt.contains(r#"CREATE TABLE "post_tags""#));
        assert!(stmt.contains(r#"PRIMARY KEY ("post_id", "tag_id")"#));
        assert!(stmt.contains(r#"CONSTRAINT "post_tags_post_id_fkey" FOREIGN KEY ("post_id") REFERENCES "posts" ("id") ON DELETE CASCADE"#));
        assert!(stmt.contains(r#"CONSTRAINT "post_tags_tag_id_fkey" FOREIGN KEY ("tag_id") REFERENCES "tags" ("id") ON DELETE CASCADE"#));
    }
}
