//! Diff two `SchemaSnapshot`s into a list of DDL statements.
//!
//! Detects added and dropped tables, columns, indexes, constraints
//! and junction tables, plus column type, nullability, default,
//! length and uniqueness changes.
//!
//! **Statement order is a contract.** Indexes, constraints and junction
//! tables drop before the columns and tables they hang off. `CREATE
//! TABLE` comes before `ADD COLUMN`, so a new column can reference a new
//! table. Tables drop child first. FK constraints for new tables come last.
//!
//! `ADD COLUMN ... NOT NULL` only works when the field has a
//! `default`, which backfills the existing rows. Without one it is
//! an error, and the message names the two fixes: make the field
//! `Option<T>`, or add `#[rustango(default = "…")]`.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::snapshot::{FieldSnapshot, RelationSnapshot, SchemaSnapshot, TableSnapshot};

fn default_index_method_diff() -> String {
    "btree".to_owned()
}

fn default_exclusion_method() -> String {
    "gist".to_owned()
}

/// One thing that must change to move from `prev` to `current`.
///
/// Serialized externally tagged, as `{"CreateTable": "foo"}` or
/// `{"AddColumn": {"table": "foo", "column": "bar"}}`. This is what
/// a migration file stores under `Operation::Schema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// Change a column's type, such as `i32` to `i64`.
    ///
    /// The types are the neutral name strings from
    /// `FieldSnapshot.ty`, not the closed `FieldType` enum, so an
    /// existing migration file keeps loading when a new type is
    /// added. `string` to `string` restates the type for a
    /// `case_insensitive` change.
    AlterColumnType {
        table: String,
        column: String,
        from: String,
        to: String,
    },
    /// Switch a column between nullable and NOT NULL. `nullable` is
    /// the **new** state.
    AlterColumnNullable {
        table: String,
        column: String,
        nullable: bool,
    },
    /// Change a column's `DEFAULT`. `Some(expr)` sets it, `None`
    /// drops it. Both sides are carried so the op inverts without a
    /// snapshot.
    AlterColumnDefault {
        table: String,
        column: String,
        from: Option<String>,
        to: Option<String>,
    },
    /// Change a string column's `max_length`, so between two
    /// `VARCHAR` sizes or between `VARCHAR(N)` and `TEXT`.
    AlterColumnMaxLength {
        table: String,
        column: String,
        from: Option<u32>,
        to: Option<u32>,
    },
    /// Change a column's `db_comment`; `None` drops it. SQLite has no
    /// comments, so it writes nothing there.
    AlterColumnComment {
        table: String,
        column: String,
        from: Option<String>,
        to: Option<String>,
    },
    /// Rename a table. `detect_changes` never emits this: a snapshot
    /// diff cannot tell a rename from a drop plus an add. Write it by
    /// hand with `manage makemigrations --empty <name>`, then edit
    /// the JSON.
    RenameTable {
        old_name: String,
        new_name: String,
    },
    /// Rename a column. Hand-authored, like `RenameTable`.
    RenameColumn {
        table: String,
        old_column: String,
        new_column: String,
    },
    /// Add or drop a `UNIQUE` constraint on one column. `unique` is
    /// the **new** state.
    AlterColumnUnique {
        table: String,
        column: String,
        unique: bool,
    },
    /// Change an FK column's `ON DELETE` action; `None` is NO ACTION.
    /// The FK is dropped and re-added; SQLite rebuilds the table (#1557).
    AlterFkOnDelete {
        table: String,
        column: String,
        from: Option<String>,
        to: Option<String>,
    },
    /// Create a `CREATE [UNIQUE] INDEX` on a model table.
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        /// Access method, lowercase: `btree`, `gin`, `gist` and so
        /// on. Missing means `btree`, so older files still load.
        #[serde(default = "default_index_method_diff")]
        method: String,
        /// `WHERE` clause for a partial index. `None` gives a plain
        /// index. **MySQL has no partial-index syntax**, so the
        /// writer drops the clause there and warns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_clause: Option<String>,
        /// Covering-index columns, for Postgres 11's `INCLUDE (...)`.
        /// **MySQL and SQLite lack it**, so the writer drops the
        /// clause there and warns.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include: Vec<String>,
    },
    /// Drop an index by name.
    ///
    /// **`table` is needed on MySQL**, whose syntax is
    /// `DROP INDEX <name> ON <table>`. PG and SQLite drop by name
    /// alone. `makemigrations` emits one `DropIndex` per index when
    /// a model goes away, so without the table those migrations
    /// cannot run on MySQL.
    ///
    /// `#[serde(default)]` keeps older files loading. They still
    /// apply on PG and SQLite, and give a clear message on MySQL
    /// rather than a serde error.
    DropIndex {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        table: String,
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
    /// Add a Postgres `EXCLUDE` constraint.
    ///
    /// **Postgres only.** MySQL and SQLite have no equivalent, so
    /// the writer emits nothing and warns, and the rest of the
    /// migration still applies.
    ///
    /// A typical booking-conflict constraint reads
    /// `EXCLUDE USING gist (room_id WITH =, during WITH &&)`.
    AddExclusionConstraint {
        name: String,
        table: String,
        /// Index method: `gist`, `btree_gist` or `spgist`. Missing
        /// means `gist`, which supports range overlap.
        #[serde(default = "default_exclusion_method")]
        using: String,
        /// `(column, operator)` pairs in declaration order, where
        /// the operator is the PG comparison for that column: `=`
        /// for equality, `&&` for range overlap, `@>` for
        /// containment.
        elements: Vec<(String, String)>,
        /// `WHERE` predicate limiting the constraint to some rows.
        /// `None` applies it to all of them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_clause: Option<String>,
    },
    /// Drop an `EXCLUDE` constraint by name. Postgres only.
    DropExclusionConstraint {
        name: String,
        table: String,
    },
    /// Create a many-to-many junction table: two `BIGINT NOT NULL`
    /// FK columns and a composite `PRIMARY KEY`.
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
    /// Add a multi-column foreign key from
    /// `#[rustango(fk_composite(...))]`. The statement is deferred
    /// to the end of the batch, so the referenced table exists by
    /// the time the constraint is created.
    AddCompositeFk {
        table: String,
        name: String,
        to: String,
        from: Vec<String>,
        on: Vec<String>,
    },
    /// Drop a composite FK by constraint name.
    DropCompositeFk {
        table: String,
        name: String,
    },
}

impl SchemaChange {
    /// The table this change writes.
    pub(crate) fn table(&self) -> &str {
        match self {
            Self::CreateTable(t)
            | Self::DropTable(t)
            | Self::DropM2MTable { through: t }
            | Self::CreateM2MTable { through: t, .. }
            | Self::RenameTable { old_name: t, .. }
            | Self::AddColumn { table: t, .. }
            | Self::DropColumn { table: t, .. }
            | Self::AlterColumnType { table: t, .. }
            | Self::AlterColumnNullable { table: t, .. }
            | Self::AlterColumnDefault { table: t, .. }
            | Self::AlterColumnMaxLength { table: t, .. }
            | Self::AlterColumnComment { table: t, .. }
            | Self::RenameColumn { table: t, .. }
            | Self::AlterColumnUnique { table: t, .. }
            | Self::AlterFkOnDelete { table: t, .. }
            | Self::CreateIndex { table: t, .. }
            | Self::DropIndex { table: t, .. }
            | Self::AddCheckConstraint { table: t, .. }
            | Self::DropCheckConstraint { table: t, .. }
            | Self::AddExclusionConstraint { table: t, .. }
            | Self::DropExclusionConstraint { table: t, .. }
            | Self::AddCompositeFk { table: t, .. }
            | Self::DropCompositeFk { table: t, .. } => t,
        }
    }

    /// Whether this change writes `table` or adds an FK to it. FK targets
    /// live in `snapshot`, the migration's after-state.
    pub(crate) fn touches(&self, table: &str, snapshot: &super::SchemaSnapshot) -> bool {
        let fk_to = |f: &FieldSnapshot| f.fk.as_ref().is_some_and(|r| r.to == table);
        self.table() == table
            || match self {
                Self::RenameTable { new_name, .. } => new_name == table,
                Self::CreateM2MTable {
                    src_table,
                    dst_table,
                    ..
                } => src_table == table || dst_table == table,
                Self::AddCompositeFk { to, .. } => to == table,
                Self::CreateTable(t) => snapshot.table(t).is_some_and(|s| {
                    s.fields.iter().any(fk_to) || s.composite_fks.iter().any(|c| c.to == table)
                }),
                Self::AddColumn { table: t, column }
                | Self::AlterFkOnDelete {
                    table: t, column, ..
                } => snapshot
                    .table(t)
                    .and_then(|s| s.field(column))
                    .is_some_and(fk_to),
                _ => false,
            }
    }
}

/// Compute the ordered list of changes from `prev` to `current`.
///
/// **The order is a contract:** drop what hangs off a table or column
/// (indexes, checks, excludes, composite FKs, then M2M junctions), create
/// tables, add columns, alter columns, drop columns, drop tables child
/// first, then create the dependents. MySQL commits each DDL statement, so
/// a drop that fails after its column or table went cannot roll back (#1879).
///
/// Table and model-column renames are never emitted: a snapshot diff cannot
/// tell them from a drop plus an add. Write those by hand with
/// `manage makemigrations --empty <name>`. A changed M2M junction column is
/// the exception: its junction stays, so it is a `RenameColumn`. Changes this cannot
/// express are reported by [`detect_unsupported_field_changes`]
/// instead of being silently skipped.
#[must_use]
pub fn detect_changes(prev: &SchemaSnapshot, current: &SchemaSnapshot) -> Vec<SchemaChange> {
    let mut changes = Vec::new();
    // Created after the tables and columns; the drop halves of changed
    // objects go in the first phase with the other drops.
    let mut creates = Vec::new();
    // Recreated composite FKs go last, after the unique index they reference.
    let mut fk_creates = Vec::new();

    // Dropped or edited composite FKs on tables that stay, before the
    // unique index they reference; a dropped table takes its own. An edit
    // that keeps the name is a Drop + Add, like an index (#1881).
    for pt in &prev.tables {
        let Some(ct) = current.table(&pt.name) else {
            continue;
        };
        for pf in &pt.composite_fks {
            let now = ct.composite_fks.iter().find(|c| c.name == pf.name);
            if now == Some(pf) {
                continue;
            }
            changes.push(SchemaChange::DropCompositeFk {
                table: pt.name.clone(),
                name: pf.name.clone(),
            });
            fk_creates.extend(now.map(|c| add_composite_fk(&ct.name, c)));
        }
    }
    // Dropped and changed indexes. A changed one (same name, new shape)
    // lowers to a Drop + Create pair, or the database keeps the old one.
    for idx in &prev.indexes {
        let changed = current.index(&idx.name).map(|c| {
            (c.columns != idx.columns
                || c.unique != idx.unique
                || c.table != idx.table
                || c.method != idx.method
                || c.where_clause != idx.where_clause
                || c.include != idx.include)
                .then_some(c)
        });
        if matches!(changed, None | Some(Some(_))) {
            // `idx.table`: an index that moved tables is dropped where it is now.
            changes.push(SchemaChange::DropIndex {
                name: idx.name.clone(),
                table: idx.table.clone(),
            });
        }
        if let Some(Some(c)) = changed {
            creates.push(create_index(c));
        }
    }
    // Dropped or edited CHECK constraints.
    for c in &prev.checks {
        let now = current.check(&c.name);
        if now != Some(c) {
            changes.push(SchemaChange::DropCheckConstraint {
                name: c.name.clone(),
                table: c.table.clone(),
            });
            creates.extend(now.map(add_check));
        }
    }
    // Dropped or edited PG EXCLUDE constraints.
    for x in &prev.excludes {
        let now = current.excludes.iter().find(|c| c.name == x.name);
        if now != Some(x) {
            changes.push(SchemaChange::DropExclusionConstraint {
                name: x.name.clone(),
                table: x.table.clone(),
            });
            creates.extend(now.map(add_exclude));
        }
    }
    // Dropped or edited M2M junctions, before the tables they reference.
    for mt in &prev.m2m_tables {
        let now = current.m2m_table(&mt.through);
        if let Some(renames) = now.filter(|c| *c != mt).and_then(|c| m2m_renames(mt, c)) {
            changes.extend(renames);
        } else if now != Some(mt) {
            changes.push(SchemaChange::DropM2MTable {
                through: mt.through.clone(),
            });
            creates.extend(now.map(create_m2m));
        }
    }

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
    // Metadata changes on columns that kept their name: type,
    // nullability, default and max_length become AlterColumn ops.
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
    // Dropped tables, each before the tables it references.
    for name in dropped_tables_child_first(prev, current) {
        changes.push(SchemaChange::DropTable(name.to_owned()));
    }

    // New indexes, then the recreated halves of changed objects.
    for idx in &current.indexes {
        if prev.index(&idx.name).is_none() {
            changes.push(create_index(idx));
        }
    }
    changes.append(&mut creates);
    // New CHECK constraints.
    for c in &current.checks {
        if prev.check(&c.name).is_none() {
            changes.push(add_check(c));
        }
    }
    // New PG EXCLUDE constraints (issue #319).
    for x in &current.excludes {
        if !prev.excludes.iter().any(|p| p.name == x.name) {
            changes.push(add_exclude(x));
        }
    }
    // New M2M junction tables.
    for mt in &current.m2m_tables {
        if prev.m2m_table(&mt.through).is_none() {
            changes.push(create_m2m(mt));
        }
    }
    // New composite FKs on existing tables. A new table's come with its
    // `CreateTable`; emitting them here too added each one twice (#1983).
    for ct in &current.tables {
        let Some(pt) = prev.table(&ct.name) else {
            continue;
        };
        let prev_fks = pt.composite_fks.as_slice();
        for cf in &ct.composite_fks {
            if !prev_fks.iter().any(|p| p.name == cf.name) {
                changes.push(add_composite_fk(&ct.name, cf));
            }
        }
    }
    changes.append(&mut fk_creates);
    changes
}

pub(super) fn create_index(idx: &super::snapshot::IndexSnapshot) -> SchemaChange {
    SchemaChange::CreateIndex {
        name: idx.name.clone(),
        table: idx.table.clone(),
        columns: idx.columns.clone(),
        unique: idx.unique,
        method: idx.method.clone(),
        where_clause: idx.where_clause.clone(),
        include: idx.include.clone(),
    }
}

fn add_check(c: &super::snapshot::CheckSnapshot) -> SchemaChange {
    SchemaChange::AddCheckConstraint {
        name: c.name.clone(),
        table: c.table.clone(),
        expr: c.expr.clone(),
    }
}

pub(super) fn add_exclude(x: &super::snapshot::ExclusionSnapshot) -> SchemaChange {
    SchemaChange::AddExclusionConstraint {
        name: x.name.clone(),
        table: x.table.clone(),
        using: x.using.clone(),
        elements: x.elements.clone(),
        where_clause: x.where_clause.clone(),
    }
}

/// The column renames that turn junction `old` into `new` over the same
/// tables, so its rows survive (#2245). `None` if an end's table changed,
/// or a self-referencing junction renamed both columns, which
/// [`detect_unsupported_field_changes`] refuses.
fn m2m_renames(
    old: &super::snapshot::M2MTableSnapshot,
    new: &super::snapshot::M2MTableSnapshot,
) -> Option<Vec<SchemaChange>> {
    let rename = |from: &str, to: &str| SchemaChange::RenameColumn {
        table: old.through.clone(),
        old_column: from.to_owned(),
        new_column: to.to_owned(),
    };
    // Self-referencing: the snapshot sorts the ends by name, so only a
    // column that keeps its name says which end is which.
    if is_self_ref(old) && is_self_ref(new) && old.src_table == new.src_table {
        let (olds, news) = ([&old.src_col, &old.dst_col], [&new.src_col, &new.dst_col]);
        let gone = olds.iter().find(|c| !news.contains(c))?;
        let came = news.iter().find(|c| !olds.contains(c))?;
        let kept = olds.iter().filter(|c| news.contains(c)).count();
        return (kept == 1).then(|| vec![rename(gone, came)]);
    }
    let direct = (old.src_table == new.src_table && old.dst_table == new.dst_table)
        .then_some([(&old.src_col, &new.src_col), (&old.dst_col, &new.dst_col)]);
    let mirrored = (old.src_table == new.dst_table && old.dst_table == new.src_table)
        .then_some([(&old.src_col, &new.dst_col), (&old.dst_col, &new.src_col)]);
    let pairs = direct.or(mirrored)?;
    let changed: Vec<_> = pairs.into_iter().filter(|(a, b)| a != b).collect();
    Some(match changed[..] {
        [(a, b)] => vec![rename(a, b)],
        // A swap goes through a spare name.
        [(a, b), (c, d)] if b == c && d == a => {
            let spare = swap_spare(a, b);
            vec![rename(a, &spare), rename(c, d), rename(&spare, b)]
        }
        // `b` is still `c`'s name until `c` moves.
        [(a, b), (c, d)] if b == c => vec![rename(c, d), rename(a, b)],
        [(a, b), (c, d)] => vec![rename(a, b), rename(c, d)],
        _ => Vec::new(),
    })
}

fn is_self_ref(m: &super::snapshot::M2MTableSnapshot) -> bool {
    m.src_table == m.dst_table
}

/// A junction column name that is neither `a` nor `b` and fits PG's 63 bytes.
fn swap_spare(a: &str, b: &str) -> String {
    let mut cut = a.len().min(56);
    while !a.is_char_boundary(cut) {
        cut -= 1;
    }
    let base = &a[..cut];
    (0..)
        .map(|i| format!("{base}_swp{i}"))
        .find(|s| s != a && s != b)
        .expect("an unbounded range finds a free name")
}

pub(super) fn create_m2m(mt: &super::snapshot::M2MTableSnapshot) -> SchemaChange {
    SchemaChange::CreateM2MTable {
        through: mt.through.clone(),
        src_table: mt.src_table.clone(),
        src_col: mt.src_col.clone(),
        dst_table: mt.dst_table.clone(),
        dst_col: mt.dst_col.clone(),
    }
}

pub(super) fn add_composite_fk(
    table: &str,
    cf: &super::snapshot::CompositeFkSnapshot,
) -> SchemaChange {
    SchemaChange::AddCompositeFk {
        table: table.to_owned(),
        name: cf.name.clone(),
        to: cf.to.clone(),
        from: cf.from.clone(),
        on: cf.on.clone(),
    }
}

/// Tables in `prev` but not `current`, each before any it references.
/// A cycle falls back to name order.
fn dropped_tables_child_first<'a>(
    prev: &'a SchemaSnapshot,
    current: &SchemaSnapshot,
) -> Vec<&'a str> {
    let references = |t: &TableSnapshot, target: &str| {
        t.name != target
            && (t
                .fields
                .iter()
                .any(|f| f.fk.as_ref().is_some_and(|r| r.to == target))
                || t.composite_fks.iter().any(|c| c.to == target))
    };
    let mut left: Vec<&TableSnapshot> = prev
        .tables
        .iter()
        .filter(|t| current.table(&t.name).is_none())
        .collect();
    let mut out = Vec::with_capacity(left.len());
    while !left.is_empty() {
        let i = left
            .iter()
            .position(|t| !left.iter().any(|o| references(o, &t.name)))
            .unwrap_or(0);
        out.push(left.remove(i).name.as_str());
    }
    out
}

/// What an FK points at, without its `ON DELETE` action.
fn fk_identity(r: &RelationSnapshot) -> (&str, &str, &str) {
    (&r.kind, &r.to, &r.on)
}

/// Same action as the database applies it: no clause is `NO ACTION` on all
/// three backends, and SQL keywords ignore case (#1573).
fn same_on_delete(p: &RelationSnapshot, c: &RelationSnapshot) -> bool {
    fn effective(r: &RelationSnapshot) -> &str {
        r.on_delete.as_deref().unwrap_or("NO ACTION")
    }
    effective(p).eq_ignore_ascii_case(effective(c))
}

fn push_alter_changes(
    table: &str,
    pf: &FieldSnapshot,
    cf: &FieldSnapshot,
    out: &mut Vec<SchemaChange>,
) {
    // A length change renders a string type, so it goes on the string side:
    // before the type when leaving a string, after it when entering one.
    // Run the other way it undid the type change (#1878).
    let max_length = (pf.max_length != cf.max_length).then(|| SchemaChange::AlterColumnMaxLength {
        table: table.to_owned(),
        column: cf.column.clone(),
        from: pf.max_length,
        to: cf.max_length,
    });
    let leaving_string = pf.ty != cf.ty && cf.ty != "string";
    if leaving_string {
        out.extend(max_length.clone());
    }
    // CITEXT, NOCASE or a `_ci` collation: a type change (#2239).
    let ci_flip = cf.ty == "string" && pf.case_insensitive != cf.case_insensitive;
    if pf.ty != cf.ty || ci_flip {
        out.push(SchemaChange::AlterColumnType {
            table: table.to_owned(),
            column: cf.column.clone(),
            from: pf.ty.clone(),
            to: cf.ty.clone(),
        });
    }
    if !leaving_string {
        out.extend(max_length);
    }
    // The default first: a SQLite rebuild to NOT NULL fills NULLs with it.
    // A type change writes the new default itself; a separate op undid
    // into `SET DEFAULT <old>` on the new type.
    if pf.default != cf.default && pf.ty == cf.ty {
        out.push(SchemaChange::AlterColumnDefault {
            table: table.to_owned(),
            column: cf.column.clone(),
            from: pf.default.clone(),
            to: cf.default.clone(),
        });
    }
    if pf.nullable != cf.nullable {
        out.push(SchemaChange::AlterColumnNullable {
            table: table.to_owned(),
            column: cf.column.clone(),
            nullable: cf.nullable,
        });
    }
    if pf.unique != cf.unique {
        out.push(SchemaChange::AlterColumnUnique {
            table: table.to_owned(),
            column: cf.column.clone(),
            unique: cf.unique,
        });
    }
    if pf.db_comment != cf.db_comment {
        out.push(SchemaChange::AlterColumnComment {
            table: table.to_owned(),
            column: cf.column.clone(),
            from: pf.db_comment.clone(),
            to: cf.db_comment.clone(),
        });
    }
    // Same FK, new action; `None → Some` included, or an upgrade never gets it (#1557).
    if let (Some(p), Some(c)) = (&pf.fk, &cf.fk) {
        if fk_identity(p) == fk_identity(c) && !same_on_delete(p, c) {
            out.push(SchemaChange::AlterFkOnDelete {
                table: table.to_owned(),
                column: cf.column.clone(),
                from: p.on_delete.clone(),
                to: c.on_delete.clone(),
            });
        }
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
    // A self-referencing junction with both columns renamed: which end is
    // which is unknown, and a Drop + Create would lose its rows (#2245).
    for pm in prev.m2m_tables.iter().filter(|m| is_self_ref(m)) {
        let Some(cm) = current.m2m_table(&pm.through) else {
            continue;
        };
        if cm != pm && is_self_ref(cm) && cm.src_table == pm.src_table {
            if m2m_renames(pm, cm).is_none() {
                out.push(format!(
                    "self-referencing M2M `{}` renamed both columns (`{}`, `{}` → `{}`, `{}`); \
                     rename one per migration, or write the RenameColumn ops by hand",
                    pm.through, pm.src_col, pm.dst_col, cm.src_col, cm.dst_col
                ));
            }
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
    // Identity only: an `on_delete` change is an `AlterFkOnDelete` op (#1557).
    if pf.fk.as_ref().map(fk_identity) != cf.fk.as_ref().map(fk_identity) {
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
    // No backend alters a generated expression in place (#2239).
    if pf.generated_as != cf.generated_as {
        out.push(format!(
            "`{table}.{col}` generated_as changed: {:?} → {:?}",
            pf.generated_as, cf.generated_as
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
        // Postgres never rebuilds.
        rebuild: _,
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
#[non_exhaustive]
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
    /// A SQLite table rebuild that runs after `immediate`, for a change
    /// the engine cannot `ALTER` in place.
    pub rebuild: Option<super::rebuild::TableRebuild>,
}

impl RenderedBatch {
    /// One rebuild per batch: a second would silently replace the first.
    fn set_rebuild(&mut self, rebuild: super::rebuild::TableRebuild) -> Result<(), String> {
        if self.rebuild.is_some() {
            return Err(format!(
                "rebuilding `{}` needs a batch of its own; render SQLite changes one at a time",
                rebuild.table()
            ));
        }
        self.rebuild = Some(rebuild);
        Ok(())
    }
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
/// It has no before-snapshot, so a MySQL column drop lacks its FK drop;
/// [`super::render_changes_between`] has one (#2026).
///
/// # Errors
/// As [`render_changes_split`].
pub fn render_changes_split_with_dialect(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Result<RenderedBatch, String> {
    render_changes_split_inner(changes, current, dialect, None, false, false)
}

/// As [`render_changes_split_with_dialect`], but every FK target is
/// qualified with `schema`, so it cannot resolve through `search_path`
/// to a same-named table elsewhere (#1645).
pub(crate) fn render_changes_split_in_schema(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Result<RenderedBatch, String> {
    render_changes_split_inner(changes, current, dialect, schema, false, true)
}

/// As [`render_changes_split_in_schema`] for tables with no rows, where a
/// NOT NULL column needs no default (#2066).
pub(crate) fn render_changes_split_for_empty(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Result<RenderedBatch, String> {
    render_changes_split_inner(changes, current, dialect, schema, true, true)
}

/// The quoted `REFERENCES` target, schema-qualified when `schema` is set.
/// A registry model's table is shared, so it resolves through `search_path` (#1718).
fn fk_target(dialect: &dyn crate::sql::Dialect, schema: Option<&str>, table: &str) -> String {
    let shared = || {
        crate::core::ModelEntry::for_table(table)
            .is_some_and(|e| e.schema.scope == crate::core::ModelScope::Registry)
    };
    match schema {
        Some(s) if !shared() => {
            format!("{}.{}", dialect.quote_ident(s), dialect.quote_ident(table))
        }
        _ => dialect.quote_ident(table),
    }
}

/// An `AlterColumn*` on MySQL or SQLite (#1676). MySQL restates the whole
/// column with `MODIFY COLUMN`; SQLite rebuilds the table into `current`.
fn alter_column_elsewhere(
    change: &SchemaChange,
    table: &str,
    f: &FieldSnapshot,
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
    unique_names: &UniqueNames,
    out: &mut RenderedBatch,
) -> Result<(), String> {
    let unique = match change {
        SchemaChange::AlterColumnUnique { unique, .. } => Some(*unique),
        _ => None,
    };
    if let Some(rebuild) = super::rebuild::TableRebuild::needed(dialect, current, table) {
        let mut rebuild = rebuild?;
        // An AddColumn's unique index would come back from the catalog,
        // under whatever name a rename left it.
        if unique == Some(false) {
            rebuild = rebuild.dropping_unique(&f.column, declared_indexes(current, table));
        }
        if !f.nullable && f.generated_as.is_none() {
            if let Some(expr) = &f.default {
                let value = render_column_default(expr, &f.ty, f.max_length, dialect);
                let col = dialect.quote_ident(&f.column);
                rebuild = rebuild.copy_as(&f.column, format!("COALESCE({col}, {value})"));
            }
        }
        return out.set_rebuild(rebuild);
    }
    // MySQL refuses to change an FK column's type (3780) or drop its index
    // (1553) under the FK; the runner drops it, and it comes back here.
    let under_fk = matches!(
        change,
        SchemaChange::AlterColumnType { .. }
            | SchemaChange::AlterColumnMaxLength { .. }
            | SchemaChange::AlterColumnUnique { unique: false, .. }
    );
    if let Some(rel) = f.fk.as_ref().filter(|_| under_fk) {
        out.deferred_fks
            .push(field_fk_sql(table, &f.column, rel, dialect, schema)?);
    }
    match unique {
        Some(true) => {
            let name = unique_names.get(table, &f.column)?;
            out.immediate
                .push(dialect.add_unique_constraint_sql(table, &name, &f.column));
        }
        // The runner drops it by its name in the catalog.
        Some(false) => {}
        None => {
            if !f.nullable && f.default.is_some() {
                out.immediate.push(fill_nulls_sql(table, f, dialect));
            }
            out.immediate.push(format!(
                "ALTER TABLE {} MODIFY COLUMN {}",
                dialect.quote_ident(table),
                column_definition(f, dialect)
            ));
        }
    }
    Ok(())
}

/// PG's `COMMENT ON COLUMN` for `f`; MySQL inlines it and SQLite has none (#2270).
fn column_comment(
    table: &str,
    f: &FieldSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Option<String> {
    let comment = f.db_comment.as_deref()?;
    dialect.column_comment_statement(table, &f.column, comment)
}

/// The indexes `current` declares on `table`, which a UNIQUE drop keeps.
pub(crate) fn declared_indexes(current: &SchemaSnapshot, table: &str) -> Vec<String> {
    current
        .indexes
        .iter()
        .filter(|i| i.table == table)
        .map(|i| i.name.clone())
        .collect()
}

/// `<column> <type> [DEFAULT …] [NOT NULL] [COMMENT …]`, as CREATE TABLE
/// writes it, for MySQL's `MODIFY COLUMN`; PK and CHECK stay where they are.
fn column_definition(f: &FieldSnapshot, dialect: &dyn crate::sql::Dialect) -> String {
    let mut sql = format!(
        "{} {}",
        dialect.quote_ident(&f.column),
        sql_type_with_dialect(f, dialect)
    );
    if let Some(expr) = &f.generated_as {
        let _ = write!(sql, " GENERATED ALWAYS AS ({expr}) STORED");
    } else if let Some(expr) = &f.default {
        let rendered = render_column_default(expr, &f.ty, f.max_length, dialect);
        let _ = write!(sql, " DEFAULT {rendered}");
    }
    if !f.nullable {
        sql.push_str(" NOT NULL");
    }
    if let Some(comment) = &f.db_comment {
        if let Some(inline) = dialect.write_inline_column_comment(comment) {
            sql.push_str(&inline);
        }
    }
    sql
}

/// Who holds each UNIQUE name in `current`. Two columns that shorten to
/// one name are refused: PG would reject the second, and SQLite's
/// `DROP INDEX` for one would drop the other's.
struct UniqueNames {
    holders: std::collections::HashMap<String, Vec<String>>,
}

impl UniqueNames {
    fn new(current: &SchemaSnapshot) -> Self {
        let mut holders: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for t in &current.tables {
            for f in t
                .fields
                .iter()
                .filter(|f| f.unique && !f.primary_key && f.generated_as.is_none())
            {
                holders
                    .entry(super::ddl::unique_constraint_name(&t.name, &f.column))
                    .or_default()
                    .push(format!("`{}.{}`", t.name, f.column));
            }
        }
        for idx in &current.indexes {
            holders
                .entry(idx.name.clone())
                .or_default()
                .push(format!("index `{}`", idx.name));
        }
        Self { holders }
    }

    /// The UNIQUE name for `table.column`, unless something else holds it.
    fn get(&self, table: &str, column: &str) -> Result<String, String> {
        let name = super::ddl::unique_constraint_name(table, column);
        let me = format!("`{table}.{column}`");
        let others: Vec<&str> = self
            .holders
            .get(&name)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|h| *h != me)
            .collect();
        if others.is_empty() {
            return Ok(name);
        }
        Err(format!(
            "the UNIQUE on {me} is named `{name}`, which {} also uses; \
             rename a table or column, or declare one as a named unique index",
            others.join(", ")
        ))
    }
}

/// Who holds each FK name in `current`, where `dialect` wants it unique:
/// per table on PG, per database on MySQL. Two FKs cut to one 63-byte
/// name are refused before any DDL runs (#2245).
struct FkNames {
    holders: std::collections::HashMap<(String, String), Vec<String>>,
    per_database: bool,
}

impl FkNames {
    fn new(current: &SchemaSnapshot, dialect: &dyn crate::sql::Dialect) -> Self {
        let mut names = Self {
            holders: std::collections::HashMap::new(),
            per_database: dialect.name() == "mysql",
        };
        // SQLite's inline FK names need not be unique.
        if dialect.inline_fks_in_create_table() {
            return names;
        }
        for t in &current.tables {
            for f in t.fields.iter().filter(|f| f.fk.is_some()) {
                let name = super::ddl::fk_constraint_name(&t.name, &f.column);
                names.hold(&t.name, name, format!("`{}.{}`", t.name, f.column));
            }
            for c in &t.composite_fks {
                names.hold(
                    &t.name,
                    c.name.clone(),
                    format!("`{}` FK `{}`", t.name, c.name),
                );
            }
        }
        for m in &current.m2m_tables {
            for col in [&m.src_col, &m.dst_col] {
                let name = super::ddl::fk_constraint_name(&m.through, col);
                names.hold(&m.through, name, format!("`{}.{col}`", m.through));
            }
        }
        names
    }

    fn key(&self, table: &str, name: String) -> (String, String) {
        let scope = if self.per_database { "" } else { table };
        (scope.to_owned(), name)
    }

    fn hold(&mut self, table: &str, name: String, holder: String) {
        let key = self.key(table, name);
        self.holders.entry(key).or_default().push(holder);
    }

    /// Fails if another FK holds the name of the one on `table.column`.
    fn check(&self, table: &str, column: &str) -> Result<(), String> {
        let name = super::ddl::fk_constraint_name(table, column);
        let me = format!("`{table}.{column}`");
        let others: Vec<&str> = self
            .holders
            .get(&self.key(table, name.clone()))
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|h| *h != me)
            .collect();
        if others.is_empty() {
            return Ok(());
        }
        Err(format!(
            "the FK on {me} is named `{name}`, which {} also uses; rename a table or column",
            others.join(", ")
        ))
    }
}

fn render_changes_split_inner(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
    empty_tables: bool,
    // The runner drops a UNIQUE by its catalog name; offline, the usual one.
    by_catalog: bool,
) -> Result<RenderedBatch, String> {
    let mut out = RenderedBatch::default();
    let unique_names = UniqueNames::new(current);
    let fk_names = FkNames::new(current, dialect);
    // Once, before the first change that writes a CITEXT column (#2240).
    let mut ci_extension = dialect.ci_text_extension_sql();
    for change in changes {
        if writes_ci_text(change, current) {
            out.immediate.extend(ci_extension.take().map(str::to_owned));
        }
        match change {
            SchemaChange::CreateTable(name) => {
                let table = current.table(name).ok_or_else(|| {
                    format!("CreateTable for `{name}` but no snapshot entry for it")
                })?;
                for f in table
                    .fields
                    .iter()
                    .filter(|f| f.unique && !f.primary_key && f.generated_as.is_none())
                {
                    unique_names.get(name, &f.column)?;
                }
                for f in table.fields.iter().filter(|f| f.fk.is_some()) {
                    fk_names.check(name, &f.column)?;
                }
                out.immediate
                    .push(create_table_sql_from_snapshot_with_dialect(table, dialect));
                for f in &table.fields {
                    out.immediate.extend(column_comment(name, f, dialect));
                }
                if !dialect.inline_fks_in_create_table() {
                    out.deferred_fks
                        .extend(constraints_sql_from_snapshot(table, dialect, schema)?);
                }
            }
            SchemaChange::DropColumn { table, column } => {
                // SQLite refuses to drop an indexed column; AddColumn's
                // unique index is the one this renderer creates (#1877).
                // Skipped when another column now holds the name.
                if let Some(name) = unique_names
                    .get(table, column)
                    .ok()
                    .filter(|_| dialect.name() == "sqlite")
                {
                    out.immediate.push(format!(
                        "DROP INDEX IF EXISTS {}",
                        dialect.quote_ident(&name)
                    ));
                }
                // SQLite's DROP COLUMN refuses a column in a table-level UNIQUE (#1982).
                if let Some(rebuild) = super::rebuild::TableRebuild::needed(dialect, current, table)
                {
                    out.set_rebuild(rebuild?.dropping(column))?;
                    continue;
                }
                out.immediate.push(format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    dialect.quote_ident(table),
                    dialect.quote_ident(column),
                ));
            }
            SchemaChange::AlterFkOnDelete { table, column, .. } => {
                if let Some(rebuild) = super::rebuild::TableRebuild::needed(dialect, current, table)
                {
                    out.set_rebuild(rebuild?)?;
                    continue;
                }
                let rel = current
                    .table(table)
                    .and_then(|t| t.field(column))
                    .and_then(|f| f.fk.as_ref())
                    .ok_or_else(|| {
                        format!("AlterFkOnDelete for `{table}.{column}` but no FK in the snapshot")
                    })?;
                // The runner drops the live FK by its catalog name first.
                out.deferred_fks
                    .push(field_fk_sql(table, column, rel, dialect, schema)?);
            }
            SchemaChange::AddColumn { table, column } => {
                let t = current.table(table).ok_or_else(|| {
                    format!("AddColumn for `{table}.{column}` but table missing in snapshot")
                })?;
                let f = t.field(column).ok_or_else(|| {
                    format!("AddColumn for `{table}.{column}` but field missing in snapshot")
                })?;
                if !f.nullable && f.default.is_none() && !empty_tables {
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
                // MySQL with a binlog refuses `ADD COLUMN … DEFAULT (UUID())` (1674).
                if dialect.name() == "mysql" && is_uuid_default(f) {
                    out.immediate
                        .extend(add_column_backfilled(table, f, dialect));
                } else if dialect.name() == "mysql" && !f.nullable && f.default.is_none() {
                    // MySQL fills a NOT NULL ADD with '' or 0; MODIFY fails on a NULL row instead.
                    out.immediate
                        .extend(add_column_then_not_null(table, f, dialect));
                } else {
                    out.immediate.push(add_column_sql(table, f, dialect));
                }
                out.immediate.extend(column_comment(table, f, dialect));
                if f.fk.is_some()
                    && dialect.inline_fks_in_create_table()
                    && inline_fk_on_add_column(f, dialect).is_none()
                {
                    out.warnings.push(format!(
                        "`{table}.{column}` is added without its FOREIGN KEY: SQLite refuses \
                         REFERENCES with a non-NULL default on a table with rows. Rebuild \
                         the table by hand to add it (#559)."
                    ));
                }
                // CREATE TABLE's UNIQUE and FK, which a bare ADD COLUMN lacks (#1877).
                if f.unique && !f.primary_key {
                    let name = unique_names.get(table, column)?;
                    out.immediate
                        .push(dialect.add_unique_constraint_sql(table, &name, column));
                }
                if let Some(rel) =
                    f.fk.as_ref()
                        .filter(|_| !dialect.inline_fks_in_create_table())
                {
                    fk_names.check(table, column)?;
                    out.deferred_fks
                        .push(field_fk_sql(table, column, rel, dialect, schema)?);
                }
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
            SchemaChange::AlterColumnType { table, column, .. }
            | SchemaChange::AlterColumnNullable { table, column, .. }
            | SchemaChange::AlterColumnDefault { table, column, .. }
            | SchemaChange::AlterColumnMaxLength { table, column, .. }
            | SchemaChange::AlterColumnUnique { table, column, .. }
            | SchemaChange::AlterColumnComment { table, column, .. }
                if dialect.alters_by_rebuild() || dialect.modifies_whole_column() =>
            {
                // VARCHAR(n) and TEXT are one affinity there, and n is never enforced (#1220).
                // Nor has it column comments.
                if dialect.alters_by_rebuild()
                    && matches!(
                        change,
                        SchemaChange::AlterColumnMaxLength { .. }
                            | SchemaChange::AlterColumnComment { .. }
                    )
                {
                    continue;
                }
                let f = current
                    .table(table)
                    .and_then(|t| t.field(column))
                    .ok_or_else(|| {
                        format!("altering `{table}.{column}` but the snapshot has no such column")
                    })?;
                alter_column_elsewhere(
                    change,
                    table,
                    f,
                    current,
                    dialect,
                    schema,
                    &unique_names,
                    &mut out,
                )?;
            }
            SchemaChange::AlterColumnType {
                table,
                column,
                from: _,
                to,
            } => {
                let field = current
                    .table(table)
                    .and_then(|t| t.field(column))
                    .filter(|f| f.ty == *to);
                // In `schema`, not whatever `search_path` finds (#2308).
                let target = fk_target(dialect, schema, table);
                // The old DEFAULT may not cast to the new type, so it goes
                // first and the new one comes back after (#2242). A serial
                // or generated column keeps its own.
                let default = field.filter(|f| !f.auto && f.generated_as.is_none());
                if default.is_some() {
                    out.immediate.push(format!(
                        r#"ALTER TABLE {target} ALTER COLUMN "{column}" DROP DEFAULT"#,
                    ));
                }
                // A string takes the field's whole type, so CITEXT (#2238) or
                // VARCHAR(n) (#2239). No `USING`: the assignment cast refuses
                // what `::VARCHAR(n)` would truncate.
                let string = field.filter(|_| to == "string");
                out.immediate.push(match string {
                    Some(f) => format!(
                        r#"ALTER TABLE {target} ALTER COLUMN "{column}" TYPE {}"#,
                        sql_type_with_dialect(f, dialect)
                    ),
                    None => {
                        let pg_to = pg_type_for_ty_name(to);
                        format!(
                            r#"ALTER TABLE {target} ALTER COLUMN "{column}" TYPE {pg_to} USING "{column}"::{pg_to}"#,
                        )
                    }
                });
                if let Some(f) = default {
                    if let Some(expr) = &f.default {
                        let value = render_column_default(expr, &f.ty, f.max_length, dialect);
                        out.immediate.push(format!(
                            r#"ALTER TABLE {target} ALTER COLUMN "{column}" SET DEFAULT {value}"#,
                        ));
                    }
                }
                // A serial's sequence keeps its old type, so i32 → i64 still stops at 2^31 (#2245).
                if field.is_some_and(|f| f.auto) && matches!(to.as_str(), "i16" | "i32" | "i64") {
                    // A tagged body, so a `$$` in a name cannot end it.
                    const TAG: &str = "$rustango_seq$";
                    if target.contains(TAG) || column.contains(TAG) {
                        return Err(format!("`{table}.{column}`: a name cannot contain `{TAG}`"));
                    }
                    out.immediate.push(format!(
                        "DO {TAG} DECLARE s text := pg_get_serial_sequence({}, {}); BEGIN \
                         IF s IS NOT NULL THEN EXECUTE format('ALTER SEQUENCE %s AS {}', s); \
                         END IF; END {TAG}",
                        dialect.quote_literal(&target),
                        dialect.quote_literal(column),
                        pg_type_for_ty_name(to),
                    ));
                }
            }
            SchemaChange::AlterColumnNullable {
                table,
                column,
                nullable,
            } => {
                // Option<T> → T with a default: fill the NULLs first, or
                // SET NOT NULL fails on them (#1881).
                let field = current.table(table).and_then(|t| t.field(column));
                if let Some(f) = field.filter(|f| !*nullable && f.default.is_some()) {
                    out.immediate.push(fill_nulls_sql(table, f, dialect));
                }
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
            } => match to {
                Some(expr) => {
                    // Empty-string default → the literal `''`, not nothing
                    // (#1161), so we don't emit `SET DEFAULT ` (invalid).
                    let rendered: &str = if expr.is_empty() { "''" } else { expr };
                    out.immediate.push(format!(
                        r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" SET DEFAULT {rendered}"#,
                    ));
                }
                None => out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" DROP DEFAULT"#,
                )),
            },
            SchemaChange::AlterColumnMaxLength {
                table,
                column,
                from: _,
                to,
            } => {
                let ci = current
                    .table(table)
                    .and_then(|t| t.field(column))
                    .is_some_and(is_ci_text);
                let pg_to = match to {
                    _ if ci => dialect.ci_text_type(*to),
                    Some(n) => format!("VARCHAR({n})"),
                    None => "TEXT".into(),
                };
                // No `USING`: a `::VARCHAR(n)` cast truncates, and without
                // it PG refuses to shrink over longer values (#1878).
                out.immediate.push(format!(
                    r#"ALTER TABLE "{table}" ALTER COLUMN "{column}" TYPE {pg_to}"#,
                ));
            }
            // An empty comment drops it on PG.
            SchemaChange::AlterColumnComment {
                table, column, to, ..
            } => out.immediate.extend(dialect.column_comment_statement(
                table,
                column,
                to.as_deref().unwrap_or(""),
            )),
            SchemaChange::AlterColumnUnique {
                table,
                column,
                unique,
            } => {
                // The runner drops the name it finds in the catalog (#2133).
                if *unique {
                    let name = unique_names.get(table, column)?;
                    out.immediate
                        .push(dialect.add_unique_constraint_sql(table, &name, column));
                } else if !by_catalog {
                    out.immediate.push(format!(
                        "ALTER TABLE {} DROP CONSTRAINT {}",
                        dialect.quote_ident(table),
                        dialect.quote_ident(&unique_names.get(table, column)?),
                    ));
                }
            }
            // Both renames are genuinely portable — MySQL and SQLite
            // (3.25+) support them — so unlike the `AlterColumn*` arms
            // above there is nothing to guard. What they need is the
            // dialect's quoting, which they did not have: the literal
            // `"` here is a string delimiter on MySQL, so
            // `ALTER TABLE "post" RENAME TO "article"` is `ERROR 1064`,
            // the same failure as #1461 in the `DropCheckConstraint`
            // arm of this same match (#559).
            //
            // Named rather than counted, because the count has now been
            // wrong twice: it said "two arms further down" (#1507), and
            // the correction said "three arms sit between them" when it
            // is four (#1606 review). A position that moves whenever an
            // arm is added does not belong in a comment.
            SchemaChange::RenameTable { old_name, new_name } => {
                out.immediate.push(format!(
                    "ALTER TABLE {} RENAME TO {}",
                    dialect.quote_ident(old_name),
                    dialect.quote_ident(new_name),
                ));
            }
            SchemaChange::RenameColumn {
                table,
                old_column,
                new_column,
            } => {
                out.immediate.push(format!(
                    "ALTER TABLE {} RENAME COLUMN {} TO {}",
                    dialect.quote_ident(table),
                    dialect.quote_ident(old_column),
                    dialect.quote_ident(new_column),
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
                    // In the table's schema, not whatever `search_path` finds first.
                    fk_target(dialect, schema, table),
                    using,
                ));
            }
            SchemaChange::DropIndex { name, table } => {
                // MySQL needs `DROP INDEX <name> ON <table>` and rejects
                // `IF EXISTS` on it. PostgreSQL and SQLite drop by name.
                if dialect.name() == "mysql" {
                    if table.is_empty() {
                        return Err(format!(
                            "DropIndex for `{name}` carries no table, and MySQL needs one \
                             (`DROP INDEX <name> ON <table>`). This migration file was \
                             written before #1588 added the field. Add \
                             `\"table\": \"<owning table>\"` beside `\"name\"` in the \
                             DropIndex op, or regenerate the migration. PostgreSQL and \
                             SQLite apply the file unchanged."
                        ));
                    }
                    out.immediate.push(format!(
                        "DROP INDEX {} ON {}",
                        dialect.quote_ident(name),
                        dialect.quote_ident(table),
                    ));
                    continue;
                }
                out.immediate.push(format!(
                    "DROP INDEX IF EXISTS {}",
                    dialect.quote_ident(name)
                ));
            }
            // SQLite takes CHECKs and composite FKs only in `CREATE TABLE` (#2127).
            SchemaChange::AddCheckConstraint { table, .. }
            | SchemaChange::DropCheckConstraint { table, .. }
            | SchemaChange::AddCompositeFk { table, .. }
            | SchemaChange::DropCompositeFk { table, .. }
                if dialect.alters_by_rebuild() =>
            {
                // A drop on a table this migration drops goes with the table.
                let dropped = matches!(
                    change,
                    SchemaChange::DropCheckConstraint { .. } | SchemaChange::DropCompositeFk { .. }
                ) && current.table(table).is_none();
                if dropped {
                    continue;
                }
                if let Some(rebuild) = super::rebuild::TableRebuild::needed(dialect, current, table)
                {
                    out.set_rebuild(rebuild?)?;
                }
            }
            SchemaChange::AddCheckConstraint { name, table, expr } => {
                out.immediate.push(format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({expr})",
                    dialect.quote_ident(table),
                    dialect.quote_ident(name),
                ));
            }
            SchemaChange::DropCheckConstraint { name, table } => {
                // The dialect owns both halves: whether it can drop a
                // constraint at all, and how it spells it. `None` is
                // SQLite saying it has no `ALTER TABLE DROP CONSTRAINT`.
                //
                // This arm used to test `dialect.name() == "sqlite"`
                // itself and then hand-write the statement — which is
                // how it came to send PostgreSQL syntax to MySQL (#559).
                let Some(sql) = dialect.drop_check_constraint_sql(table, name) else {
                    return Err(format!(
                        "DropCheckConstraint for `{table}.{name}` is not yet supported on \
                         dialect `{}`. That dialect has no `ALTER TABLE DROP CONSTRAINT` \
                         syntax. Workaround: emit a hand-written `Operation::Data` (RunSQL) \
                         that rebuilds the table without the CHECK. Tracked in #559.",
                        dialect.name()
                    ));
                };
                out.immediate.push(sql);
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
                fk_names.check(through, src_col)?;
                fk_names.check(through, dst_col)?;
                let q_through = dialect.quote_ident(through);
                let q_src_col = dialect.quote_ident(src_col);
                let q_dst_col = dialect.quote_ident(dst_col);

                if dialect.inline_fks_in_create_table() {
                    // SQLite: ALTER TABLE … ADD CONSTRAINT FK isn't supported.
                    // Emit the FK clauses inside the CREATE TABLE statement.
                    let q_src_table = fk_target(dialect, schema, src_table);
                    let q_dst_table = fk_target(dialect, schema, dst_table);
                    let q_id = dialect.quote_ident("id");
                    let q_src_fk =
                        dialect.quote_ident(&super::ddl::fk_constraint_name(through, src_col));
                    let q_dst_fk =
                        dialect.quote_ident(&super::ddl::fk_constraint_name(through, dst_col));
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
                    out.deferred_fks
                        .push(m2m_fk_sql(through, src_col, src_table, dialect, schema));
                    out.deferred_fks
                        .push(m2m_fk_sql(through, dst_col, dst_table, dialect, schema));
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
                out.deferred_fks
                    .push(composite_fk_sql(table, name, to, from, on, dialect, schema));
            }
            SchemaChange::DropCompositeFk { table, name } => {
                // Same single owner as the CHECK arm above.
                let Some(sql) = dialect.drop_foreign_key_sql(table, name) else {
                    return Err(format!(
                        "DropCompositeFk for `{table}.{name}` is not yet supported on \
                         dialect `{}`. That dialect has no `ALTER TABLE DROP CONSTRAINT` \
                         syntax. Workaround: emit a hand-written `Operation::Data` (RunSQL) \
                         that rebuilds the table without the FK. Tracked in #559.",
                        dialect.name()
                    ));
                };
                out.immediate.push(sql);
            }
        }
    }
    Ok(out)
}

/// A case-insensitive string column, typed by `dialect.ci_text_type`.
fn is_ci_text(f: &FieldSnapshot) -> bool {
    f.case_insensitive && f.ty == "string"
}

/// Whether `change` gives a column its case-insensitive type.
fn writes_ci_text(change: &SchemaChange, current: &SchemaSnapshot) -> bool {
    let field = |t: &str, c: &str| current.table(t).and_then(|t| t.field(c));
    match change {
        SchemaChange::CreateTable(t) => current
            .table(t)
            .is_some_and(|t| t.fields.iter().any(is_ci_text)),
        SchemaChange::AddColumn { table, column }
        | SchemaChange::AlterColumnType { table, column, .. }
        | SchemaChange::AlterColumnMaxLength { table, column, .. } => {
            field(table, column).is_some_and(is_ci_text)
        }
        _ => false,
    }
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
        // #342 — PG hstore.
        "hstore" => "hstore".into(),
        other => other.to_uppercase(),
    }
}

fn create_table_sql_from_snapshot_with_dialect(
    t: &TableSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> String {
    create_table_sql_as(t, &t.name, dialect)
}

/// `CREATE TABLE <name>` in `t`'s shape; constraint names still follow `t.name`.
pub(super) fn create_table_sql_as(
    t: &TableSnapshot,
    name: &str,
    dialect: &dyn crate::sql::Dialect,
) -> String {
    let mut sql = format!("CREATE TABLE {} (", dialect.quote_ident(name));
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
            let rendered = render_column_default(expr, &f.ty, f.max_length, dialect);
            let _ = write!(sql, " DEFAULT {rendered}");
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
                sql.push_str(&inline_references(rel, dialect));
            }
        }
    }
    // Named, table-level: `AlterColumnUnique` drops it by this name (#1880).
    for f in &t.fields {
        if f.unique && !f.primary_key && f.generated_as.is_none() {
            sql.push_str(", ");
            sql.push_str(&super::ddl::unique_clause(dialect, &t.name, &f.column));
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
    schema: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = t
        .fields
        .iter()
        .filter_map(|f| {
            f.fk.as_ref()
                .map(|rel| field_fk_sql(&t.name, &f.column, rel, dialect, schema))
        })
        .collect::<Result<_, _>>()?;
    out.extend(
        composite_fks(t, dialect, schema)
            .into_iter()
            .map(|(_, sql)| sql),
    );
    Ok(out)
}

/// Each composite FK of `t` by name, with its `ADD CONSTRAINT … FOREIGN KEY`.
pub(crate) fn composite_fks(
    t: &TableSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Vec<(String, String)> {
    t.composite_fks
        .iter()
        .map(|cf| {
            let sql =
                composite_fk_sql(&t.name, &cf.name, &cf.to, &cf.from, &cf.on, dialect, schema);
            (cf.name.clone(), sql)
        })
        .collect()
}

fn composite_fk_sql(
    table: &str,
    name: &str,
    to: &str,
    from: &[String],
    on: &[String],
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> String {
    let cols = |cs: &[String]| {
        cs.iter()
            .map(|c| dialect.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        dialect.quote_ident(table),
        dialect.quote_ident(name),
        cols(from),
        fk_target(dialect, schema, to),
        cols(on),
    )
}

/// Each column of `t` with its `ADD CONSTRAINT … FOREIGN KEY`, if it has one.
pub(crate) fn column_fks(
    t: &TableSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Result<Vec<(String, Option<String>)>, String> {
    t.fields
        .iter()
        .map(|f| {
            let fk = f.fk.as_ref();
            let sql = fk
                .map(|rel| field_fk_sql(&t.name, &f.column, rel, dialect, schema))
                .transpose()?;
            Ok((f.column.clone(), sql))
        })
        .collect()
}

/// The FK of a column: a model field's, or a junction column's.
pub(crate) enum ColumnFk<'a> {
    Field(&'a RelationSnapshot),
    /// The junction end's table.
    Junction(&'a str),
}

/// The FK `table.column` has in `snap`, if any.
pub(crate) fn column_fk<'a>(
    snap: &'a SchemaSnapshot,
    table: &str,
    column: &str,
) -> Option<ColumnFk<'a>> {
    let field = snap.table(table).and_then(|t| t.field(column));
    if let Some(rel) = field.and_then(|f| f.fk.as_ref()) {
        return Some(ColumnFk::Field(rel));
    }
    snap.m2m_table(table).and_then(|m| {
        [(&m.src_col, &m.src_table), (&m.dst_col, &m.dst_table)]
            .into_iter()
            .find_map(|(c, to)| (c == column).then_some(ColumnFk::Junction(to)))
    })
}

/// The `ADD CONSTRAINT … FOREIGN KEY` of `table.column` in `snap`, a model's or a junction's.
pub(crate) fn column_fk_sql(
    snap: &SchemaSnapshot,
    table: &str,
    column: &str,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Result<Option<String>, String> {
    column_fk(snap, table, column)
        .map(|fk| match fk {
            ColumnFk::Field(rel) => field_fk_sql(table, column, rel, dialect, schema),
            ColumnFk::Junction(to) => Ok(m2m_fk_sql(table, column, to, dialect, schema)),
        })
        .transpose()
}

/// A junction column's FK, which cascades.
fn m2m_fk_sql(
    through: &str,
    column: &str,
    to: &str,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE CASCADE",
        dialect.quote_ident(through),
        dialect.quote_ident(&super::ddl::fk_constraint_name(through, column)),
        dialect.quote_ident(column),
        fk_target(dialect, schema, to),
        dialect.quote_ident("id"),
    )
}

/// ` REFERENCES <to> (<on>) [ON DELETE …]`, for SQLite's inline FKs.
fn inline_references(rel: &RelationSnapshot, dialect: &dyn crate::sql::Dialect) -> String {
    let mut s = format!(
        " REFERENCES {} ({})",
        dialect.quote_ident(&rel.to),
        dialect.quote_ident(&rel.on),
    );
    // #1549 — the declared action, or the constraint lands as
    // NO ACTION and a declared cascade becomes a refusal.
    if let Some(action) = &rel.on_delete {
        let _ = write!(s, " ON DELETE {action}");
    }
    s
}

/// `ALTER TABLE … ADD CONSTRAINT <table>_<column>_fkey FOREIGN KEY …`.
fn field_fk_sql(
    table: &str,
    column: &str,
    rel: &RelationSnapshot,
    dialect: &dyn crate::sql::Dialect,
    schema: Option<&str>,
) -> Result<String, String> {
    let mut s = format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        dialect.quote_ident(table),
        dialect.quote_ident(&super::ddl::fk_constraint_name(table, column)),
        dialect.quote_ident(column),
        fk_target(dialect, schema, &rel.to),
        dialect.quote_ident(&rel.on),
    );
    // #1549 — this is the path system migrations take, and
    // it was silently dropping the declared action.
    if let Some(action) = &rel.on_delete {
        // Accepted and then not enforced: refuse it rather than migrate a lie.
        let set_default = crate::core::OnDeleteAction::SetDefault.as_sql();
        if action.eq_ignore_ascii_case(set_default) && !dialect.supports_on_delete_set_default() {
            return Err(format!(
                "`{table}.{column}`: on_delete = \"set_default\" is not enforced on {} \
                 (the parent delete is refused); use another action (#1573)",
                dialect.name()
            ));
        }
        let _ = write!(s, " ON DELETE {action}");
    }
    Ok(s)
}

/// Render a column `DEFAULT` expression for CREATE TABLE / ADD COLUMN.
///
/// An empty-string default (`#[rustango(default = "")]`) means the literal
/// empty string — render it as `''` rather than a blank, or we emit
/// `DEFAULT  NOT NULL` which the driver rejects (#1161). The `''` literal is
/// then routed through [`Dialect::translate_default_expr`] like any other
/// expression so a MySQL LOB column (TEXT/JSON/BLOB) gets the parenthesized
/// expression form `DEFAULT ('')` it requires — MySQL rejects a *literal*
/// default on those types (error 1101), only the 8.0.13+ expression form is
/// legal. PG/SQLite leave `''` untouched (#1174).
fn render_column_default(
    expr: &str,
    ty: &str,
    max_length: Option<u32>,
    dialect: &dyn crate::sql::Dialect,
) -> String {
    let expr_to_render = if expr.is_empty() { "''" } else { expr };
    dialect.translate_default_expr(expr_to_render, ty, max_length)
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
        let rendered = render_column_default(expr, &f.ty, f.max_length, dialect);
        let _ = write!(sql, " DEFAULT {rendered}");
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
    // MySQL's comment; PG's comes after, as `COMMENT ON COLUMN` (#2270).
    sql.push_str(&inline_comment(f, dialect));
    // SQLite cannot `ADD CONSTRAINT`; its FK rides on the column (#1877).
    if let Some(rel) = inline_fk_on_add_column(f, dialect) {
        sql.push_str(&inline_references(rel, dialect));
    }
    sql
}

/// `true` for a column whose DEFAULT is a random UUID (`gen_random_uuid()`).
pub(crate) fn is_uuid_default(f: &FieldSnapshot) -> bool {
    f.default.as_deref().is_some_and(crate::sql::is_uuid_expr)
}

/// `f` added nullable with no DEFAULT, a fresh UUID per row, then the
/// DEFAULT and NOT NULL set by `MODIFY` (MySQL only).
fn add_column_backfilled(
    table: &str,
    f: &FieldSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Vec<String> {
    let bare = FieldSnapshot {
        default: None,
        nullable: true,
        ..f.clone()
    };
    let value = render_column_default(
        f.default.as_deref().unwrap_or_default(),
        &f.ty,
        f.max_length,
        dialect,
    );
    let null = if f.nullable { "" } else { " NOT NULL" };
    vec![
        add_column_sql(table, &bare, dialect),
        fill_nulls_sql(table, f, dialect),
        format!(
            "ALTER TABLE {} MODIFY COLUMN {} {} DEFAULT {value}{null}{}",
            dialect.quote_ident(table),
            dialect.quote_ident(&f.column),
            sql_type_with_dialect(f, dialect),
            inline_comment(f, dialect),
        ),
    ]
}

/// MySQL's inline ` COMMENT '…'` for `f`; a `MODIFY` without it drops the comment.
fn inline_comment(f: &FieldSnapshot, dialect: &dyn crate::sql::Dialect) -> String {
    f.db_comment
        .as_deref()
        .and_then(|c| dialect.write_inline_column_comment(c))
        .unwrap_or_default()
}

/// `f` added nullable, then made NOT NULL by `MODIFY` (MySQL only).
fn add_column_then_not_null(
    table: &str,
    f: &FieldSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Vec<String> {
    let bare = FieldSnapshot {
        nullable: true,
        ..f.clone()
    };
    vec![
        add_column_sql(table, &bare, dialect),
        format!(
            "ALTER TABLE {} MODIFY COLUMN {} {} NOT NULL{}",
            dialect.quote_ident(table),
            dialect.quote_ident(&f.column),
            sql_type_with_dialect(f, dialect),
            inline_comment(f, dialect),
        ),
    ]
}

/// `UPDATE` setting each NULL `f` to its DEFAULT, evaluated per row.
pub(crate) fn fill_nulls_sql(
    table: &str,
    f: &FieldSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> String {
    let value = render_column_default(
        f.default.as_deref().unwrap_or_default(),
        &f.ty,
        f.max_length,
        dialect,
    );
    format!(
        "UPDATE {t} SET {c} = {value} WHERE {c} IS NULL",
        t = dialect.quote_ident(table),
        c = dialect.quote_ident(&f.column),
    )
}

/// SQLite's inline FK for `ADD COLUMN`. With `foreign_keys=ON` SQLite
/// refuses `REFERENCES` beside a non-NULL default once the table has rows.
fn inline_fk_on_add_column<'a>(
    f: &'a FieldSnapshot,
    dialect: &dyn crate::sql::Dialect,
) -> Option<&'a RelationSnapshot> {
    f.fk.as_ref()
        .filter(|_| dialect.inline_fks_in_create_table() && f.default.is_none())
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
        // #342 — PG hstore.
        "hstore" => Some(FieldType::HStore),
        _ => None,
    };
    // v0.13.2: `auto = true` historically meant "PK SERIAL/BIGSERIAL"
    // for integer types; field-mixin auto (auto_now_add etc.) on
    // non-integer types falls through to the regular column_type
    // mapping. Dialect-specific token is picked by `dialect.serial_type()`.
    if let Some(t @ (FieldType::I16 | FieldType::I32 | FieldType::I64)) = ty {
        if f.auto {
            return dialect.serial_type(t).to_owned();
        }
        // As `ddl::sql_type`: a DB default the PK type would ignore (#2137).
        if f.primary_key && f.default.is_some() {
            if let Some(pk_ty) = dialect.defaulted_integer_pk_type() {
                return pk_ty.to_owned();
            }
        }
    }
    // #344 — case-insensitive String columns route through
    // `dialect.ci_text_type` (PG → CITEXT, SQLite → TEXT COLLATE
    // NOCASE, MySQL → LONGTEXT COLLATE utf8mb4_general_ci).
    if is_ci_text(f) {
        return dialect.ci_text_type(f.max_length);
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

    /// Added NOT NULL, MySQL would fill a row that slipped in after the empty check.
    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_not_null_add_on_empty_table_tightens_after() {
        let snap = SchemaSnapshot {
            tables: vec![TableSnapshot {
                name: "t".into(),
                model: "T".into(),
                fields: vec![fs("i32", false)],
                composite_fks: Vec::new(),
            }],
            ..Default::default()
        };
        let add = [SchemaChange::AddColumn {
            table: "t".into(),
            column: "x".into(),
        }];
        let out = render_changes_split_for_empty(&add, &snap, &crate::sql::MySql, None).unwrap();
        assert_eq!(out.immediate.len(), 2, "{:?}", out.immediate);
        assert!(
            !out.immediate[0].contains("NOT NULL"),
            "{}",
            out.immediate[0]
        );
        assert!(
            out.immediate[1].starts_with("ALTER TABLE `t` MODIFY COLUMN `x` ")
                && out.immediate[1].ends_with(" NOT NULL"),
            "{}",
            out.immediate[1]
        );
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

    /// #2137: an `INTEGER` PK is SQLite's rowid, which skips the default.
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_defaulted_integer_pk_is_not_the_rowid() {
        let mut f = fs("i64", false);
        f.primary_key = true;
        f.default = Some("7".into());
        assert_eq!(sql_type_with_dialect(&f, &crate::sql::Sqlite), "BIGINT");
        f.default = None;
        assert_eq!(sql_type_with_dialect(&f, &crate::sql::Sqlite), "INTEGER");
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

        // Bound inside the gate: nothing else reads it, so at
        // `--no-default-features --features sqlite` it is an unused
        // binding rather than a fixture (#1370).
        #[cfg(feature = "mysql")]
        {
            let drop_col = [SchemaChange::DropColumn {
                table: "t".into(),
                column: "c".into(),
            }];
            let my =
                render_changes_split_with_dialect(&drop_col, &snap, &crate::sql::MySql).unwrap();
            assert_eq!(
                my.immediate,
                vec!["ALTER TABLE `t` DROP COLUMN `c`".to_string()]
            );
        }
    }

    /// `a_b.c` and `a.b_c` both shorten to `a_b_c_key`.
    fn clashing_uniques() -> SchemaSnapshot {
        let uniq = |t: &str, c: &str| TableSnapshot {
            name: t.into(),
            model: t.into(),
            fields: vec![FieldSnapshot {
                name: c.into(),
                column: c.into(),
                unique: true,
                nullable: true,
                ..fs("i64", false)
            }],
            composite_fks: vec![],
        };
        SchemaSnapshot {
            tables: vec![uniq("a_b", "c"), uniq("a", "b_c")],
            ..SchemaSnapshot::default()
        }
    }

    #[test]
    fn clashing_unique_names_are_refused() {
        use crate::migrate::SchemaChange;
        let snap = clashing_uniques();
        let err = render_changes_split_with_dialect(
            &[SchemaChange::CreateTable("a_b".into())],
            &snap,
            &crate::sql::Postgres,
        )
        .expect_err("two UNIQUEs named a_b_c_key");
        assert!(
            err.contains("`a_b_c_key`") && err.contains("`a.b_c`"),
            "{err}"
        );
    }

    /// A dropped `a_b.c` must not drop `a.b_c`'s index.
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_drop_column_keeps_another_tables_unique_index() {
        use crate::migrate::SchemaChange;
        let mut snap = clashing_uniques();
        snap.tables[0].fields.clear();
        let drop = [SchemaChange::DropColumn {
            table: "a_b".into(),
            column: "c".into(),
        }];
        let out = render_changes_split_with_dialect(&drop, &snap, &crate::sql::Sqlite).unwrap();
        assert!(out.immediate.is_empty(), "{:?}", out.immediate);
        assert_eq!(out.rebuild.as_ref().map(|r| r.table()), Some("a_b"));
        snap.tables.remove(1);
        let out = render_changes_split_with_dialect(&drop, &snap, &crate::sql::Sqlite).unwrap();
        assert_eq!(out.immediate, [r#"DROP INDEX IF EXISTS "a_b_c_key""#]);
    }

    /// SQLite changes an FK action by rebuilding; PG and MySQL re-add the FK
    /// after the runner drops the live one (#1557).
    #[test]
    fn alter_fk_on_delete_renders_per_dialect() {
        use crate::migrate::{RelationSnapshot, SchemaChange};
        let mut child = TableSnapshot {
            name: "c".into(),
            model: "c".into(),
            fields: vec![FieldSnapshot {
                name: "p_id".into(),
                column: "p_id".into(),
                fk: Some(RelationSnapshot {
                    kind: "fk".into(),
                    to: "p".into(),
                    on: "id".into(),
                    on_delete: Some("CASCADE".into()),
                }),
                ..fs("i64", false)
            }],
            composite_fks: vec![],
        };
        child.fields.insert(0, fs("i64", true));
        let snap = SchemaSnapshot {
            tables: vec![child],
            ..SchemaSnapshot::default()
        };
        let alter = [SchemaChange::AlterFkOnDelete {
            table: "c".into(),
            column: "p_id".into(),
            from: None,
            to: Some("CASCADE".into()),
        }];
        let pg = render_changes_split_with_dialect(&alter, &snap, &crate::sql::Postgres).unwrap();
        assert!(pg.immediate.is_empty() && pg.rebuild.is_none());
        assert_eq!(
            pg.deferred_fks,
            [
                r#"ALTER TABLE "c" ADD CONSTRAINT "c_p_id_fkey" FOREIGN KEY ("p_id") REFERENCES "p" ("id") ON DELETE CASCADE"#
            ]
        );
        #[cfg(feature = "sqlite")]
        {
            let sq = render_changes_split_with_dialect(&alter, &snap, &crate::sql::Sqlite).unwrap();
            assert!(sq.immediate.is_empty() && sq.deferred_fks.is_empty());
            let twice = [alter[0].clone(), alter[0].clone()];
            assert!(render_changes_split_with_dialect(&twice, &snap, &crate::sql::Sqlite).is_err());
            let stmts = sq
                .rebuild
                .expect("a rebuild")
                .statements(&crate::sql::Sqlite);
            assert!(stmts[0].starts_with(r#"CREATE TABLE "_rustango_rebuild_c""#));
            assert!(stmts[0].contains("ON DELETE CASCADE"), "{}", stmts[0]);
            assert_eq!(
                stmts[stmts.len() - 2..],
                [
                    r#"DROP TABLE "c""#,
                    r#"ALTER TABLE "_rustango_rebuild_c" RENAME TO "c""#
                ]
            );
        }
    }

    /// InnoDB ignores `SET DEFAULT`, so MySQL refuses to render it (#1573).
    #[test]
    fn set_default_is_refused_where_not_enforced() {
        use crate::migrate::{RelationSnapshot, SchemaChange};
        let child = TableSnapshot {
            name: "c".into(),
            model: "c".into(),
            fields: vec![FieldSnapshot {
                name: "p_id".into(),
                column: "p_id".into(),
                fk: Some(RelationSnapshot {
                    kind: "fk".into(),
                    to: "p".into(),
                    on: "id".into(),
                    on_delete: Some("SET DEFAULT".into()),
                }),
                ..fs("i64", false)
            }],
            composite_fks: vec![],
        };
        let snap = SchemaSnapshot {
            tables: vec![child],
            ..SchemaSnapshot::default()
        };
        let create = [SchemaChange::CreateTable("c".into())];
        let pg = render_changes_split_with_dialect(&create, &snap, &crate::sql::Postgres).unwrap();
        assert!(pg.deferred_fks[0].ends_with("ON DELETE SET DEFAULT"));
        #[cfg(feature = "mysql")]
        {
            let err = render_changes_split_with_dialect(&create, &snap, &crate::sql::MySql)
                .expect_err("MySQL accepts SET DEFAULT and then refuses the delete");
            assert!(err.contains("set_default"), "{err}");
        }
    }

    // -------- AlterColumn* per dialect (#1676) --------

    fn empty_snap() -> SchemaSnapshot {
        SchemaSnapshot::default()
    }

    /// `t(id, c)` with `c` NOT NULL DEFAULT 7.
    fn alter_snap() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![TableSnapshot {
                name: "t".into(),
                model: "t".into(),
                fields: vec![
                    FieldSnapshot {
                        primary_key: true,
                        ..fs("i64", true)
                    },
                    FieldSnapshot {
                        name: "c".into(),
                        column: "c".into(),
                        default: Some("7".into()),
                        ..fs("i64", false)
                    },
                ],
                composite_fks: vec![],
            }],
            ..SchemaSnapshot::default()
        }
    }

    #[cfg(any(feature = "mysql", feature = "sqlite"))]
    fn not_null() -> Vec<SchemaChange> {
        vec![SchemaChange::AlterColumnNullable {
            table: "t".into(),
            column: "c".into(),
            nullable: false,
        }]
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

    /// `t(id, email, name)`, `email` case-insensitive.
    fn ci_snap() -> SchemaSnapshot {
        let string = |name: &str, ci: bool| FieldSnapshot {
            name: name.into(),
            column: name.into(),
            max_length: Some(100),
            case_insensitive: ci,
            nullable: true,
            ..fs("string", false)
        };
        SchemaSnapshot {
            tables: vec![TableSnapshot {
                name: "t".into(),
                model: "t".into(),
                fields: vec![
                    FieldSnapshot {
                        primary_key: true,
                        ..fs("i64", true)
                    },
                    string("email", true),
                    string("name", false),
                ],
                composite_fks: vec![],
            }],
            ..SchemaSnapshot::default()
        }
    }

    fn add(column: &str) -> SchemaChange {
        SchemaChange::AddColumn {
            table: "t".into(),
            column: column.into(),
        }
    }

    /// The `citext` prelude comes once, before the first CITEXT column (#2240).
    #[test]
    fn citext_extension_precedes_the_first_citext_column() {
        let render = |changes: &[SchemaChange], dialect: &dyn crate::sql::Dialect| {
            render_changes_split_with_dialect(changes, &ci_snap(), dialect)
                .unwrap()
                .immediate
        };
        let prelude = "CREATE EXTENSION IF NOT EXISTS citext SCHEMA public;";
        let pg = render(
            &[
                add("name"),
                SchemaChange::CreateTable("t".into()),
                add("email"),
            ],
            &crate::sql::Postgres,
        );
        assert_eq!(pg.iter().filter(|s| *s == prelude).count(), 1, "{pg:?}");
        assert_eq!(pg[1], prelude, "{pg:?}");
        assert!(!render(&[add("name")], &crate::sql::Postgres).contains(&prelude.to_owned()));
        #[cfg(feature = "sqlite")]
        assert!(!render(&[add("email")], &crate::sql::Sqlite).contains(&prelude.to_owned()));
    }

    /// MySQL restates the whole column, NULLs filled first.
    #[cfg(feature = "mysql")]
    #[test]
    fn alter_column_is_a_modify_on_mysql() {
        let out = render_changes_split_with_dialect(&not_null(), &alter_snap(), &crate::sql::MySql)
            .unwrap();
        assert_eq!(
            out.immediate,
            [
                "UPDATE `t` SET `c` = 7 WHERE `c` IS NULL",
                "ALTER TABLE `t` MODIFY COLUMN `c` BIGINT DEFAULT 7 NOT NULL"
            ]
        );
    }

    /// SQLite rebuilds, copying NULLs as the default.
    #[cfg(feature = "sqlite")]
    #[test]
    fn alter_column_is_a_rebuild_on_sqlite() {
        let out =
            render_changes_split_with_dialect(&not_null(), &alter_snap(), &crate::sql::Sqlite)
                .unwrap();
        assert!(out.immediate.is_empty());
        let stmts = out
            .rebuild
            .expect("a rebuild")
            .statements(&crate::sql::Sqlite);
        assert!(
            stmts[1].contains(r#"SELECT "x", COALESCE("c", 7) FROM "t""#),
            "{}",
            stmts[1]
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn alter_column_max_length_is_a_no_op_on_sqlite() {
        // #1220. This used to be `..._errors_on_sqlite`, asserting a
        // rejection. SQLite gives `VARCHAR(n)` and `TEXT` the same
        // affinity and never enforces the length, so there is no DDL
        // to emit and nothing to fail — refusing it failed a
        // migration over a change that does nothing.
        let snap = empty_snap();
        let changes = vec![SchemaChange::AlterColumnMaxLength {
            table: "t".into(),
            column: "c".into(),
            from: Some(50),
            to: Some(100),
        }];
        let batch = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite)
            .expect("AlterColumnMaxLength must be accepted on SQLite");
        assert!(
            batch.immediate.is_empty() && batch.deferred_fks.is_empty(),
            "no statement should be emitted, got {:?} / {:?}",
            batch.immediate,
            batch.deferred_fks,
        );
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

    /// A MySQL `MODIFY` restates the whole column, so it keeps the comment.
    #[test]
    fn mysql_add_column_modify_keeps_the_comment() {
        let snap: SchemaSnapshot = serde_json::from_value(serde_json::json!({ "tables": [{
            "name": "t", "model": "T", "fields": [
                { "name": "c", "column": "c", "ty": "i64", "nullable": false,
                  "primary_key": false, "db_comment": "kept" },
                { "name": "u", "column": "u", "ty": "uuid", "nullable": false,
                  "primary_key": false, "default": "gen_random_uuid()",
                  "db_comment": "kept" }] }] }))
        .unwrap();
        for column in ["c", "u"] {
            let add = [SchemaChange::AddColumn {
                table: "t".into(),
                column: column.into(),
            }];
            let out =
                render_changes_split_for_empty(&add, &snap, &crate::sql::MySql, None).unwrap();
            let modify = out.immediate.iter().find(|s| s.contains("MODIFY COLUMN"));
            assert!(
                modify.is_some_and(|s| s.ends_with("COMMENT 'kept'")),
                "{:?}",
                out.immediate
            );
        }
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
            table: "post".into(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        // PostgreSQL drops by name; the table is carried for MySQL's
        // sake and must not leak into this form.
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
            table: "post".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::Sqlite).unwrap();
        assert_eq!(
            out.immediate,
            vec![r#"DROP INDEX IF EXISTS "idx_post_slug""#.to_string()]
        );
    }

    /// MySQL renders `DROP INDEX <name> ON <table>` — the form it
    /// actually accepts.
    ///
    /// This test replaces one that asserted the opposite. The old
    /// `drop_index_mysql_rejects_until_variant_carries_table` pinned
    /// the refusal as correct behaviour, so the bug in #1588 had a
    /// green test defending it: `makemigrations` generated `DropIndex`
    /// for every index of a dropped model, and the result was
    /// un-appliable on MySQL straight out of the generator.
    #[cfg(feature = "mysql")]
    #[test]
    fn drop_index_mysql_names_the_table() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropIndex {
            name: "idx_post_slug".into(),
            table: "post".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql)
            .expect("DropIndex renders on MySQL now that the variant carries the table");
        // No `IF EXISTS` — MySQL rejects it on DROP INDEX.
        assert_eq!(
            out.immediate,
            vec!["DROP INDEX `idx_post_slug` ON `post`".to_string()]
        );
    }

    /// A migration file written before #1588 carries no `table`, and
    /// `#[serde(default)]` gives it an empty one. On MySQL that cannot
    /// be rendered, and the error has to name the file's problem rather
    /// than emit `ON ``` and let the server complain.
    #[cfg(feature = "mysql")]
    #[test]
    fn drop_index_mysql_without_a_table_says_which_file_to_fix() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropIndex {
            name: "idx_post_slug".into(),
            table: String::new(),
        }];
        let err = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql)
            .expect_err("an empty table cannot render on MySQL");
        assert!(err.contains("idx_post_slug"), "{err}");
        assert!(err.contains("carries no table"), "{err}");
        assert!(err.contains("\"table\""), "names the field to add: {err}");
    }

    /// …and the same legacy file still applies on PostgreSQL, because
    /// that dialect never needed the table. An upgrade must not break
    /// migrations that were working.
    #[test]
    fn drop_index_without_a_table_still_renders_on_postgres() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropIndex {
            name: "idx_post_slug".into(),
            table: String::new(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(
            out.immediate,
            vec![r#"DROP INDEX IF EXISTS "idx_post_slug""#.to_string()]
        );
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

    /// Offline renders drop a UNIQUE by its usual name; the runner's by the catalog's (#2133).
    #[test]
    fn unique_drop_is_rendered_offline_only() {
        let drop = vec![SchemaChange::AlterColumnUnique {
            table: "t".into(),
            column: "c".into(),
            unique: false,
        }];
        let snap = alter_snap();
        let offline = render_changes(&drop, &snap).unwrap();
        assert_eq!(
            offline,
            vec![r#"ALTER TABLE "t" DROP CONSTRAINT "t_c_key""#.to_string()]
        );
        let runner =
            render_changes_split_in_schema(&drop, &snap, &crate::sql::Postgres, None).unwrap();
        assert!(runner.immediate.is_empty(), "{:?}", runner.immediate);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn check_constraints_rebuild_on_sqlite() {
        let mut snap = alter_snap();
        snap.checks.push(super::super::snapshot::CheckSnapshot {
            name: "ck_t_c".into(),
            table: "t".into(),
            expr: "c >= 0".into(),
        });
        let add = vec![SchemaChange::AddCheckConstraint {
            name: "ck_t_c".into(),
            table: "t".into(),
            expr: "c >= 0".into(),
        }];
        let batch = render_changes_split_with_dialect(&add, &snap, &crate::sql::Sqlite).unwrap();
        let sql = batch
            .rebuild
            .expect("a rebuild")
            .statements(&crate::sql::Sqlite);
        assert!(
            sql[0].contains(r#"CONSTRAINT "ck_t_c" CHECK (c >= 0)"#),
            "{sql:?}"
        );
        snap.checks.clear();
        let drop = vec![SchemaChange::DropCheckConstraint {
            name: "ck_t_c".into(),
            table: "t".into(),
        }];
        let batch = render_changes_split_with_dialect(&drop, &snap, &crate::sql::Sqlite).unwrap();
        let sql = batch
            .rebuild
            .expect("a rebuild")
            .statements(&crate::sql::Sqlite);
        assert!(!sql[0].contains("CHECK"), "{sql:?}");
        // Its table is dropped in the same migration: nothing to rebuild.
        let gone =
            render_changes_split_with_dialect(&drop, &empty_snap(), &crate::sql::Sqlite).unwrap();
        assert!(gone.rebuild.is_none() && gone.immediate.is_empty());
    }

    /// MySQL spells a check drop `DROP CHECK`, with no `IF EXISTS`.
    ///
    /// This test previously asserted
    /// `` ALTER TABLE `t` DROP CONSTRAINT IF EXISTS `ck_x` ``, which
    /// MySQL 8.0.46 rejects outright:
    ///
    /// ```text
    /// ERROR 1064 (42000): ... right syntax to use near 'IF EXISTS `ck_x`'
    /// ```
    ///
    /// It was named `..._uses_backticks` and it did check the quoting —
    /// the statement around the quoting was simply never run against a
    /// server. That is the whole hazard of an emission test: it proves
    /// the writer emitted what its author intended, never that the
    /// server accepts it (#1461).
    #[cfg(feature = "mysql")]
    #[test]
    fn drop_check_constraint_mysql_uses_drop_check() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCheckConstraint {
            name: "ck_x".into(),
            table: "t".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql).unwrap();
        assert_eq!(
            out.immediate,
            vec!["ALTER TABLE `t` DROP CHECK `ck_x`".to_string()]
        );
        assert!(
            !out.immediate[0].contains("IF EXISTS"),
            "MySQL parses no `IF EXISTS` on a constraint drop"
        );
    }

    /// Postgres keeps the idempotent form; only the MySQL arm changed.
    #[test]
    fn drop_check_constraint_postgres_keeps_if_exists() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCheckConstraint {
            name: "ck_x".into(),
            table: "t".into(),
        }];
        let out =
            render_changes_split_with_dialect(&changes, &snap, &crate::sql::Postgres).unwrap();
        assert_eq!(
            out.immediate,
            vec![r#"ALTER TABLE "t" DROP CONSTRAINT IF EXISTS "ck_x""#.to_string()]
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

    /// #1645 — the m2m and composite arms qualify their targets too.
    #[test]
    fn in_schema_render_qualifies_every_fk_target() {
        let snap = empty_snap();
        let mut changes = make_create_m2m();
        changes.extend(make_add_composite_fk());
        let out =
            render_changes_split_in_schema(&changes, &snap, &crate::sql::Postgres, Some("t1"))
                .unwrap();
        assert!(out.deferred_fks[0].contains(r#"REFERENCES "t1"."posts" ("id")"#));
        assert!(out.deferred_fks[1].contains(r#"REFERENCES "t1"."tags" ("id")"#));
        assert!(out.deferred_fks[2].contains(r#"REFERENCES "t1"."parent" ("a_id", "b_id")"#));
    }

    /// #2308 — the widened PK's sequence is looked up in the schema.
    #[test]
    fn in_schema_render_qualifies_the_sequence_widen() {
        let snap: SchemaSnapshot = serde_json::from_value(serde_json::json!({ "tables": [{
            "name": "item", "model": "Item", "fields": [{
                "name": "id", "column": "id", "ty": "i64", "nullable": false,
                "primary_key": true, "auto": true }] }] }))
        .unwrap();
        let widen = [SchemaChange::AlterColumnType {
            table: "item".into(),
            column: "id".into(),
            from: "i32".into(),
            to: "i64".into(),
        }];
        let out = render_changes_split_in_schema(&widen, &snap, &crate::sql::Postgres, Some("t1"))
            .unwrap();
        let seq = out
            .immediate
            .iter()
            .find(|s| s.contains("pg_get_serial_sequence"));
        assert!(
            seq.is_some_and(|s| s.contains(r#"pg_get_serial_sequence('"t1"."item"', 'id')"#)),
            "{:?}",
            out.immediate
        );
        assert!(
            out.immediate[0].starts_with(r#"ALTER TABLE "t1"."item" ALTER COLUMN"#),
            "and the ALTER beside it: {:?}",
            out.immediate
        );
    }

    /// #1645 — the plain per-field FK arm qualifies its target.
    #[test]
    fn in_schema_render_qualifies_a_field_fk() {
        let table: TableSnapshot = serde_json::from_value(serde_json::json!({
            "name": "child", "model": "Child", "fields": [{
                "name": "user_id", "column": "user_id", "ty": "i64",
                "nullable": false, "primary_key": false,
                "fk": { "kind": "fk", "to": "rustango_users", "on": "id" },
            }],
        }))
        .unwrap();
        let pg = &crate::sql::Postgres;
        let fk = constraints_sql_from_snapshot(&table, pg, Some("t1")).unwrap();
        assert!(
            fk[0].contains(r#"REFERENCES "t1"."rustango_users" ("id")"#),
            "{fk:?}"
        );
        let fk = constraints_sql_from_snapshot(&table, pg, None).unwrap();
        assert!(
            fk[0].contains(r#"REFERENCES "rustango_users" ("id")"#),
            "{fk:?}"
        );
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

    /// MySQL spells an FK drop `DROP FOREIGN KEY`, with no `IF EXISTS`.
    ///
    /// Same story as `drop_check_constraint_mysql_uses_drop_check`: this
    /// asserted the Postgres shape, which is error 1064 on MySQL. The
    /// correct branch already existed one module away, in
    /// `ddl::drop_constraints_sql_with_dialect`, and `diff.rs` did not
    /// use it (#1461).
    #[cfg(feature = "mysql")]
    #[test]
    fn drop_composite_fk_mysql_uses_drop_foreign_key() {
        let snap = empty_snap();
        let changes = vec![SchemaChange::DropCompositeFk {
            table: "child".into(),
            name: "fk_x".into(),
        }];
        let out = render_changes_split_with_dialect(&changes, &snap, &crate::sql::MySql).unwrap();
        assert_eq!(
            out.immediate,
            vec!["ALTER TABLE `child` DROP FOREIGN KEY `fk_x`".to_string()]
        );
        assert!(
            !out.immediate[0].contains("IF EXISTS"),
            "MySQL parses no `IF EXISTS` on a constraint drop"
        );
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
