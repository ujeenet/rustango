//! SQLite's table rebuild: the one path for every change it cannot `ALTER`
//! (#1557, #1982), after <https://sqlite.org/lang_altertable.html#otheralter>.
//!
//! The runner turns FK enforcement off before the transaction, as step 1
//! requires: inside one the pragma is a no-op, and `DROP TABLE` would then
//! cascade into the rows that reference the table. A rename with
//! `legacy_alter_table` cannot dodge that: with FKs on it still rewrites
//! the children's `REFERENCES`.

use super::snapshot::{SchemaSnapshot, TableSnapshot};
use crate::sql::Dialect;

/// Rebuild one table into `target`'s shape: create it under a new name,
/// copy the rows, drop the old table, rename, then re-create its indexes
/// and triggers. Inbound FKs name the table, so they follow the rename.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRebuild {
    target: TableSnapshot,
    /// Live columns the rebuild may leave behind; any other one refuses it.
    dropping: Vec<String>,
}

impl TableRebuild {
    /// A rebuild into `target`. Every stored target column is copied by name,
    /// so the old table must have each one.
    #[must_use]
    pub(crate) fn new(target: &TableSnapshot) -> Self {
        Self {
            target: target.clone(),
            dropping: Vec::new(),
        }
    }

    /// Let the rebuild leave the live `column` behind.
    #[must_use]
    pub(crate) fn dropping(mut self, column: &str) -> Self {
        self.dropping.push(column.to_owned());
        self
    }

    /// `Some` where `dialect` changes `table` by rebuilding it into its shape
    /// in `after`.
    pub(crate) fn needed(
        dialect: &dyn Dialect,
        after: &SchemaSnapshot,
        table: &str,
    ) -> Option<Result<Self, String>> {
        dialect.alters_by_rebuild().then(|| {
            after
                .table(table)
                .map(Self::new)
                .ok_or_else(|| format!("no snapshot entry for `{table}` to rebuild it from"))
        })
    }

    /// The table being rebuilt.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.target.name
    }

    /// Steps 4 to 7: create the new table, copy the rows, drop the old one,
    /// rename. Indexes and triggers are the runner's, read from the catalog.
    /// SQLite only: it writes `sqlite_sequence`.
    #[must_use]
    pub(crate) fn statements(&self, dialect: &dyn Dialect) -> Vec<String> {
        let old = &self.target.name;
        let new = format!("_rustango_rebuild_{old}");
        let cols = self
            .target
            .fields
            .iter()
            .filter(|f| f.generated_as.is_none())
            .map(|f| dialect.quote_ident(&f.column))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = vec![
            super::diff::create_table_sql_as(&self.target, &new, dialect),
            format!(
                "INSERT INTO {} ({cols}) SELECT {cols} FROM {}",
                dialect.quote_ident(&new),
                dialect.quote_ident(old)
            ),
        ];
        let autoincrement =
            dialect.serial_type_includes_primary_key()
                && self.target.fields.iter().any(|f| {
                    f.auto && f.primary_key && matches!(f.ty.as_str(), "i16" | "i32" | "i64")
                });
        if autoincrement {
            // Keep the high-water mark, or ids of deleted rows come back.
            let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
            out.push(format!(
                "DELETE FROM sqlite_sequence WHERE name = {}",
                lit(&new)
            ));
            out.push(format!(
                "INSERT INTO sqlite_sequence (name, seq) \
                 SELECT {}, seq FROM sqlite_sequence WHERE name = {}",
                lit(&new),
                lit(old)
            ));
        }
        out.push(format!("DROP TABLE {}", dialect.quote_ident(old)));
        out.push(format!(
            "ALTER TABLE {} RENAME TO {}",
            dialect.quote_ident(&new),
            dialect.quote_ident(old)
        ));
        out
    }

    /// UNIQUE names the new `CREATE TABLE` carries itself.
    #[cfg(feature = "sqlite")]
    fn inline_uniques(&self) -> Vec<String> {
        self.target
            .fields
            .iter()
            .filter(|f| f.unique && !f.primary_key && f.generated_as.is_none())
            .map(|f| super::ddl::unique_constraint_name(&self.target.name, &f.column))
            .collect()
    }

    /// Run the rebuild in `tx`, whose connection has FK enforcement off.
    #[cfg(feature = "sqlite")]
    pub(crate) async fn run(&self, tx: &mut RebuildTx<'_>) -> Result<(), super::MigrateError> {
        if !tx.fks_off {
            return Err(super::MigrateError::Validation(format!(
                "rebuilding `{}` needs FOREIGN KEY enforcement off",
                self.table()
            )));
        }
        // A column the snapshot doesn't know would be lost without a word.
        let live: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
            .bind(self.table())
            .fetch_all(&mut *tx.tx)
            .await?;
        if let Some(lost) = live
            .iter()
            .find(|c| !self.dropping.contains(c) && self.target.field(c).is_none())
        {
            return Err(super::MigrateError::Validation(format!(
                "rebuilding `{}` would lose its column `{lost}`, which the migration's \
                 snapshot does not have; add it to the model or drop it first",
                self.table()
            )));
        }
        // Step 3: what DROP TABLE takes with it, minus what CREATE re-adds.
        let saved: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, sql FROM sqlite_master \
             WHERE tbl_name = ? AND type IN ('index', 'trigger') AND sql IS NOT NULL \
             ORDER BY type = 'trigger', rowid",
        )
        .bind(self.table())
        .fetch_all(&mut *tx.tx)
        .await?;
        let inline = self.inline_uniques();
        let before = self.orphans(tx).await?;
        for stmt in self.statements(&crate::sql::Sqlite) {
            sqlx::query(&stmt).execute(&mut *tx.tx).await?;
        }
        for (_, sql) in saved.iter().filter(|(n, _)| !inline.contains(n)) {
            sqlx::query(sql).execute(&mut *tx.tx).await?;
        }
        // Step 10, for this table and its referrers: only a row the rebuild
        // orphaned refuses it, not an old one elsewhere.
        let new: Vec<_> = self
            .orphans(tx)
            .await?
            .into_iter()
            .filter(|o| !before.contains(o))
            .collect();
        if let Some((table, rowid, parent)) = new.first() {
            return Err(super::MigrateError::Validation(format!(
                "rebuilding `{}` left {} row(s) whose FOREIGN KEY points at no row, \
                 first `{table}` rowid {rowid:?} -> `{parent}`",
                self.table(),
                new.len()
            )));
        }
        Ok(())
    }

    /// `PRAGMA foreign_key_check` rows of this table and the tables that
    /// reference it; none when FK enforcement was off to begin with.
    #[cfg(feature = "sqlite")]
    async fn orphans(
        &self,
        tx: &mut RebuildTx<'_>,
    ) -> Result<Vec<(String, Option<i64>, String)>, sqlx::Error> {
        if !tx.check {
            return Ok(Vec::new());
        }
        let mut tables: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT m.name FROM sqlite_master m, pragma_foreign_key_list(m.name) f \
             WHERE m.type = 'table' AND f.\"table\" = ?",
        )
        .bind(self.table())
        .fetch_all(&mut *tx.tx)
        .await?;
        tables.push(self.table().to_owned());
        let mut out = Vec::new();
        for t in &tables {
            let rows: Vec<(String, Option<i64>, String, i64)> =
                sqlx::query_as("SELECT * FROM pragma_foreign_key_check(?)")
                    .bind(t)
                    .fetch_all(&mut *tx.tx)
                    .await?;
            out.extend(rows.into_iter().map(|(t, r, p, _)| (t, r, p)));
        }
        Ok(out)
    }
}

/// `table`'s shape after the op that `later` follows, from the migration's
/// final `after`: later renames, added columns and FK actions undone. A
/// later op this cannot undo is refused rather than rebuilt past.
pub(crate) fn shape_at(
    table: &str,
    later: &[super::Operation],
    after: &SchemaSnapshot,
) -> Result<TableSnapshot, String> {
    use super::SchemaChange as SC;
    // The table's name when each later op runs.
    let mut name = table.to_owned();
    let mut named = Vec::new();
    for op in later {
        let super::Operation::Schema(change) = op else {
            continue;
        };
        named.push((change, name.clone()));
        if let SC::RenameTable { old_name, new_name } = change {
            if *old_name == name {
                name.clone_from(new_name);
            }
        }
    }
    let mut t = after
        .table(&name)
        .cloned()
        .ok_or_else(|| format!("no snapshot entry for `{name}` to rebuild `{table}` from"))?;
    t.name = table.to_owned();
    for (change, name) in named.into_iter().rev() {
        if change.table() != name {
            continue;
        }
        match change {
            SC::AddColumn { column, .. } => t.fields.retain(|f| f.column != *column),
            SC::RenameColumn {
                old_column,
                new_column,
                ..
            } => {
                if let Some(f) = t.fields.iter_mut().find(|f| f.column == *new_column) {
                    f.column.clone_from(old_column);
                }
            }
            SC::AlterFkOnDelete { column, from, .. } => {
                if let Some(rel) = t
                    .fields
                    .iter_mut()
                    .find(|f| f.column == *column)
                    .and_then(|f| f.fk.as_mut())
                {
                    rel.on_delete.clone_from(from);
                }
            }
            // Gone by then: the rebuild may drop it early.
            SC::DropColumn { .. }
            | SC::RenameTable { .. }
            | SC::CreateIndex { .. }
            | SC::DropIndex { .. } => {}
            other => {
                return Err(format!(
                    "`{table}` is rebuilt before a later `{other:?}` on it in the same \
                     migration; move that change to a migration of its own"
                ))
            }
        }
    }
    Ok(t)
}

/// A pooled SQLite connection for migration DDL, FK enforcement off when it
/// may rebuild (steps 1 and 12). Dropped unfinished, it closes rather than
/// return to the pool with enforcement off.
#[cfg(feature = "sqlite")]
pub(crate) struct RebuildConn {
    conn: sqlx::pool::PoolConnection<sqlx::Sqlite>,
    fks_off: bool,
    was_on: bool,
    finished: bool,
}

#[cfg(feature = "sqlite")]
impl RebuildConn {
    /// A connection from `pool`; with `rebuilds`, FK enforcement turned off.
    pub(crate) async fn acquire(
        pool: &sqlx::SqlitePool,
        rebuilds: bool,
    ) -> Result<Self, sqlx::Error> {
        let mut conn = pool.acquire().await?;
        let mut was_on = false;
        if rebuilds {
            was_on = sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
                .fetch_one(&mut *conn)
                .await?
                == 1;
            sqlx::query("PRAGMA foreign_keys = OFF")
                .execute(&mut *conn)
                .await?;
        }
        Ok(Self {
            conn,
            fks_off: rebuilds,
            was_on,
            finished: false,
        })
    }

    pub(crate) async fn begin(&mut self) -> Result<RebuildTx<'_>, sqlx::Error> {
        use sqlx::Connection as _;
        Ok(RebuildTx {
            tx: self.conn.begin().await?,
            fks_off: self.fks_off,
            check: self.was_on,
        })
    }

    /// Step 12: FK enforcement back as it was.
    pub(crate) async fn finish(mut self) -> Result<(), sqlx::Error> {
        if self.was_on {
            sqlx::query("PRAGMA foreign_keys = ON")
                .execute(&mut *self.conn)
                .await?;
        }
        self.finished = true;
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
impl Drop for RebuildConn {
    fn drop(&mut self) {
        if !self.finished && self.fks_off {
            self.conn.close_on_drop();
        }
    }
}

/// The migration transaction on a [`RebuildConn`].
#[cfg(feature = "sqlite")]
pub(crate) struct RebuildTx<'c> {
    tx: sqlx::Transaction<'c, sqlx::Sqlite>,
    fks_off: bool,
    /// FK enforcement was on, so a rebuild checks what it orphaned.
    check: bool,
}

#[cfg(feature = "sqlite")]
impl RebuildTx<'_> {
    pub(crate) async fn commit(self) -> Result<(), super::MigrateError> {
        self.tx.commit().await?;
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
impl std::ops::Deref for RebuildTx<'_> {
    type Target = sqlx::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

#[cfg(feature = "sqlite")]
impl std::ops::DerefMut for RebuildTx<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tx
    }
}
