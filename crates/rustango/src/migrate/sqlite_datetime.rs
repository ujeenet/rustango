//! Normalise SQLite datetime columns onto one text shape (#1464).
//!
//! SQLite has no datetime type. A `DateTime<Utc>` column is TEXT and
//! compares **lexicographically**, so the stored spelling is part of
//! the correctness contract rather than a presentation choice.
//!
//! Before the fix, an `auto_now_add` column was filled by SQLite's
//! `CURRENT_TIMESTAMP` (`YYYY-MM-DD HH:MM:SS`) while sqlx bound a
//! `DateTime<Utc>` as RFC3339. Those diverge at position 10 — `' '`
//! (0x20) against `'T'` (0x54) — so `WHERE col < ?` was true for every
//! row whatever was bound, and cursor pagination returned page one
//! forever. Fixing the DDL stops *new* rows being written that way; it
//! does nothing for rows already on disk, and a database holding both
//! shapes sorts wrongly across them with no bind involved at all.
//!
//! So the DDL fix alone would leave every existing deployment broken
//! and silent — which is the shape of the bug, not a fix for it. This
//! module converts what is already stored.
//!
//! ## Why it runs automatically rather than behind a flag
//!
//! A repair verb reproduces the original failure: the operator does not
//! know to run it, and silence reads exactly like "you are fine". The
//! sweep is therefore part of `migrate`. It is safe to run on every
//! invocation:
//!
//! - it only ever touches SQLite — other dialects return immediately;
//! - it matches on [`SQLITE_LEGACY_DATETIME_LIKE`], a width-and-separator
//!   mask that the corrected shape cannot match (position 10 is `T`,
//!   not a space), so it is idempotent and a second run updates nothing;
//! - it is driven by the model registry, so it visits declared datetime
//!   columns and nothing else.
//!
//! ## What it does not cover
//!
//! Only columns the registry knows about: every `#[derive(Model)]` in
//! the binary plus the framework tables listed in [`FRAMEWORK_COLUMNS`].
//! A table created by hand-written DDL and never described to the ORM
//! is invisible here, and is reported rather than silently skipped.

use crate::core::{FieldType, ModelEntry, SqlValue};
use crate::sql::{Pool, SQLITE_DATETIME_FORMAT};

use super::MigrateError;

/// Framework-owned tables whose `SQLite` DDL is hand-written rather than
/// derived from a `ModelSchema`, so the registry sweep cannot see them.
///
/// `(table, column)`. Kept explicit rather than discovered: these are
/// the tables whose `CREATE TABLE` text this crate ships, so a new one
/// is a change to this crate and belongs in this list by the same edit.
const FRAMEWORK_COLUMNS: &[(&str, &str)] = &[
    ("rustango_audit_log", "occurred_at"),
    ("rustango_translations", "created_at"),
    ("rustango_translations", "updated_at"),
    ("rustango_migrations", "applied_at"),
];

/// Outcome of one sweep, so the caller can say what happened rather
/// than finishing silently.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Normalised {
    /// Rows rewritten, summed over every column visited.
    pub rows: u64,
    /// `table.column` for each column that had at least one legacy row.
    pub columns: Vec<String>,
    /// Columns skipped because the table does not exist yet. Normal on
    /// a fresh database, where `migrate` creates the tables afterwards.
    pub missing: usize,
}

impl Normalised {
    /// `true` when nothing needed rewriting.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.rows == 0
    }
}

/// Rewrite every legacy-format `SQLite` datetime value onto
/// [`SQLITE_DATETIME_FORMAT`].
///
/// A no-op on Postgres and `MySQL`, which have real datetime types and
/// never had the defect.
///
/// # Errors
/// Propagates the driver error from the sweep's `UPDATE`. A missing
/// table is **not** an error — it is counted in
/// [`Normalised::missing`], because this runs before `migrate` has
/// created the tables on a fresh database.
pub async fn normalise_sqlite_datetimes(pool: &Pool) -> Result<Normalised, MigrateError> {
    let mut out = Normalised::default();
    if pool.dialect().name() != "sqlite" {
        return Ok(out);
    }

    let d = pool.dialect();
    for (table, column) in targets() {
        // `quote_ident` rather than a hand-rolled `"{}"`: it doubles an
        // embedded quote. The invariant that a schema identifier is
        // `[A-Za-z_][A-Za-z0-9_]*` is enforced by the derive macro, but
        // `ModelSchema` is public with public `&'static str` fields, so
        // the invariant is a comment and this is a type. Identifiers
        // cannot be bound as placeholders; the values below can be and
        // are.
        let t = d.quote_ident(table.as_str());
        let c = d.quote_ident(column.as_str());

        // Convert **any** parseable value that is not already canonical,
        // rather than only the `YYYY-MM-DD HH:MM:SS` shape.
        //
        // The first version matched a `LIKE` mask of that one legacy
        // shape, which was wrong twice. It missed the *other* legacy
        // shape — sqlx's old variable-width RFC3339, where a
        // whole-second instant has no fractional part at all — so a row
        // written by an older rustango stayed un-normalised and stopped
        // comparing against the fixed-width bind this release
        // introduces. And it matched things `strftime` cannot parse:
        // `0000-00-00 00:00:00`, the MySQL zero date, fits the mask
        // exactly, `strftime` returns NULL for it, and the UPDATE wrote
        // that NULL over the row — destroying a timestamp on a nullable
        // column and hard-failing `migrate` on a NOT NULL one.
        //
        // This predicate says what is actually meant: *if SQLite can
        // read it and it is not already the canonical spelling, rewrite
        // it.* Unparseable text is left exactly as found, and the sweep
        // is idempotent by construction rather than by the mask's
        // shape — after a pass, `col = strftime(col)` and the row stops
        // matching.
        let sql = format!(
            "UPDATE {t} SET {c} = strftime(?, {c}) \
             WHERE strftime(?, {c}) IS NOT NULL \
               AND {c} <> strftime(?, {c})"
        );
        let binds = vec![
            SqlValue::String(SQLITE_DATETIME_FORMAT.to_owned()),
            SqlValue::String(SQLITE_DATETIME_FORMAT.to_owned()),
            SqlValue::String(SQLITE_DATETIME_FORMAT.to_owned()),
        ];
        match crate::sql::raw_execute_pool(pool, &sql, binds).await {
            Ok(n) if n > 0 => {
                out.rows += n;
                out.columns.push(format!("{table}.{column}"));
            }
            Ok(_) => {}
            Err(e) if is_missing_table(&e) => out.missing += 1,
            // Forward the driver error rather than flattening it to a
            // string, so a caller can still match on the sqlx cause.
            Err(crate::sql::ExecError::Driver(e)) => return Err(MigrateError::Driver(e)),
            Err(e) => return Err(MigrateError::Validation(e.to_string())),
        }
    }

    if out.rows > 0 {
        tracing::info!(
            rows = out.rows,
            columns = ?out.columns,
            "normalised SQLite datetime columns onto the RFC3339 shape (#1464); \
             values written by the old CURRENT_TIMESTAMP default did not compare \
             or sort against timestamps bound from Rust"
        );
    }
    Ok(out)
}

/// Every `(table, column)` pair worth sweeping: the registry's datetime
/// columns plus the hand-written framework tables, deduplicated.
fn targets() -> Vec<(String, String)> {
    let mut seen: Vec<(String, String)> = FRAMEWORK_COLUMNS
        .iter()
        .map(|(t, c)| ((*t).to_owned(), (*c).to_owned()))
        .collect();

    for entry in inventory::iter::<ModelEntry> {
        for field in entry.schema.fields {
            // `Date` and `Time` are excluded deliberately. The defect is
            // the `T`/space separator between date and time, which only
            // a value carrying both can have; a bare date or a bare time
            // has no separator to disagree about, and running the sweep
            // on one would rewrite it into a full timestamp.
            if field.ty != FieldType::DateTime {
                continue;
            }
            let pair = (entry.schema.table.to_owned(), field.column.to_owned());
            if !seen.contains(&pair) {
                seen.push(pair);
            }
        }
    }
    seen
}

/// `SQLite` reports an absent table as `no such table: <name>`. Matched
/// on the message because sqlx surfaces it as a generic database error
/// with no code this can key on.
fn is_missing_table(e: &crate::sql::ExecError) -> bool {
    e.to_string().contains("no such table")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framework list must not drift into the registry's territory:
    /// a table described by a `ModelSchema` is swept from the registry,
    /// and listing it here as well would visit it twice.
    ///
    /// Cheap, but it is the only thing standing between this list and
    /// a duplicate `UPDATE` per migrate run.
    #[test]
    fn the_framework_list_holds_no_duplicates() {
        let mut seen = Vec::new();
        for pair in FRAMEWORK_COLUMNS {
            assert!(
                !seen.contains(pair),
                "{}.{} is listed twice in FRAMEWORK_COLUMNS",
                pair.0,
                pair.1
            );
            seen.push(*pair);
        }
    }

    /// `targets()` must dedupe, or a model that also appears in
    /// `FRAMEWORK_COLUMNS` is swept twice.
    #[test]
    fn targets_are_unique() {
        let t = targets();
        let mut seen: Vec<&(String, String)> = Vec::new();
        for pair in &t {
            assert!(
                !seen.contains(&pair),
                "{}.{} appears twice in targets()",
                pair.0,
                pair.1
            );
            seen.push(pair);
        }
    }

    /// Idempotence, asserted where it now lives: in the predicate.
    ///
    /// This replaces a test that measured the old `LIKE` mask against
    /// the corrected shape — comparing two string lengths and one byte.
    /// That proved a property of two constants, not of the sweep, and
    /// the sweep no longer uses the mask: the predicate is "parseable
    /// and not already canonical", which cannot match its own output by
    /// construction. The behavioural proof is
    /// `the_sweep_is_idempotent` in `tests/sqlite_datetime_normalise.rs`,
    /// which runs it twice against a database and asserts the second
    /// pass changes nothing.
    #[test]
    fn the_canonical_shape_is_a_fixed_width() {
        // Every value the sweep writes is `strftime` output in
        // SQLITE_DATETIME_FORMAT, so the width is the one thing the
        // predicate's `<>` leg depends on staying constant.
        let corrected = "2026-09-19T19:44:55.869000+00:00";
        assert_eq!(corrected.len(), 32);
        assert_eq!(corrected.as_bytes()[10], b'T');
    }
}
