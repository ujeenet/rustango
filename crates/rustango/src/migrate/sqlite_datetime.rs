//! Normalise SQLite datetime columns onto one text shape.
//!
//! SQLite has no datetime type. A `DateTime<Utc>` column is TEXT and
//! compares **lexicographically**, so the stored spelling decides
//! whether a comparison is correct.
//!
//! Old rows used SQLite's `CURRENT_TIMESTAMP` shape
//! (`YYYY-MM-DD HH:MM:SS`), while a `DateTime<Utc>` bound from Rust is
//! RFC3339 with a `T`. Those two differ at character 10, so `WHERE col
//! < ?` matched every row and cursor pagination never moved past page
//! one. New DDL writes the right shape, but rows already on disk keep
//! the old one, and a database holding both sorts wrongly even with no
//! bind involved. This module rewrites what is already stored.
//!
//! ## Why it runs automatically
//!
//! A separate repair command would hit the same problem: nobody knows
//! to run it, and silence looks like health. So the sweep is part of
//! `migrate`, and it is safe on every run:
//!
//! - other dialects return at once;
//! - it rewrites a value only when `strftime` can parse it *and* it
//!   does not already match [`crate::sql::SQLITE_CANONICAL_GLOB`], so a
//!   second run changes nothing;
//! - it visits only datetime columns the model registry declares.
//!
//! ## What it does not cover
//!
//! Only columns the registry knows: every `#[derive(Model)]` in the
//! binary, plus the framework tables in `FRAMEWORK_COLUMNS`. A table
//! built by hand-written DDL that the ORM never saw is skipped, and the
//! skip is reported rather than hidden.

use crate::core::{FieldType, ModelEntry, SqlValue};
use crate::sql::{Pool, SQLITE_CANONICAL_GLOB, SQLITE_DATETIME_FORMAT};

use super::MigrateError;

/// `(table, column)` pairs whose `SQLite` DDL this crate writes by
/// hand, so the registry sweep cannot find them.
///
/// Listed by hand on purpose: this crate ships their `CREATE TABLE`
/// text, so adding one is an edit here too.
const FRAMEWORK_COLUMNS: &[(&str, &str)] = &[
    // `Translation` is a model, but these two columns exist only in
    // the hand-written DDL, so the registry does not list them.
    ("rustango_translations", "created_at"),
    ("rustango_translations", "updated_at"),
    // `rustango_jobs` has hand-written DDL and no model at all.
    ("rustango_jobs", "run_at"),
    ("rustango_jobs", "locked_at"),
    ("rustango_jobs", "created_at"),
];

/// Outcome of one sweep, so the caller can report what happened.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Normalised {
    /// Rows rewritten, summed over every column visited.
    pub rows: u64,
    /// `table.column` for each column that had at least one legacy row.
    pub columns: Vec<String>,
    /// Targets skipped because the table or column is not there yet.
    /// Normal on a fresh database, and normal after deploying a model
    /// with a new `DateTime` field but before its migration runs: the
    /// registry lists the column as soon as the binary is built.
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
/// Does nothing on Postgres and `MySQL`, which have real datetime
/// types.
///
/// # Errors
/// Returns the driver error from the sweep's `UPDATE`. A missing table
/// or column is **not** an error; it is counted in
/// [`Normalised::missing`], because both are normal part-way through a
/// migration run.
pub async fn normalise_sqlite_datetimes(pool: &Pool) -> Result<Normalised, MigrateError> {
    let mut out = Normalised::default();
    if pool.dialect().name() != "sqlite" {
        return Ok(out);
    }

    let d = pool.dialect();
    for (table, column) in targets() {
        // Identifiers cannot be bound as placeholders, so quote them.
        // `quote_ident` doubles any embedded quote; `ModelSchema` has
        // public fields, so the name is not guaranteed safe on its own.
        let t = d.quote_ident(table.as_str());
        let c = d.quote_ident(column.as_str());

        // Convert any value `strftime` can parse that is not already
        // canonical. Two rules matter here:
        //
        // 1. Match by parseability, not by a `LIKE` mask of the old
        //    shape. A mask misses sqlx's older variable-width RFC3339,
        //    and it matches junk such as the MySQL zero date
        //    `0000-00-00 00:00:00`, for which `strftime` returns NULL
        //    and the UPDATE would then wipe the value.
        // 2. Test "already canonical" by **shape**, with `GLOB`, never
        //    by `col <> strftime(FMT, col)`. SQLite's `%f` is
        //    milliseconds while chrono writes microseconds, so
        //    `strftime` is not the identity on a correct row: `.413681`
        //    comes back `.414000` and almost every good row would be
        //    rewritten on every run.
        //
        // `GLOB` keeps the sweep idempotent: a converted row stops
        // matching, and microsecond precision is left alone.
        let sql = format!(
            "UPDATE {t} SET {c} = strftime(?, {c}) \
             WHERE strftime(?, {c}) IS NOT NULL \
               AND {c} NOT GLOB ?"
        );
        let binds = vec![
            SqlValue::String(SQLITE_DATETIME_FORMAT.to_owned()),
            SqlValue::String(SQLITE_DATETIME_FORMAT.to_owned()),
            SqlValue::String(SQLITE_CANONICAL_GLOB.to_owned()),
        ];
        match crate::sql::raw_execute_pool(pool, &sql, binds).await {
            Ok(n) if n > 0 => {
                out.rows += n;
                out.columns.push(format!("{table}.{column}"));
            }
            Ok(_) => {}
            Err(e) if is_missing_target(&e) => out.missing += 1,
            // Keep the driver error whole so a caller can still match
            // on the sqlx cause.
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

    // The two migration ledgers, taken from the constants that own
    // their names so a typo cannot silently skip them.
    //
    // `ensure_ledger_pool_with_ledger` lets an operator rename a
    // ledger, so only the defaults are covered. A renamed ledger is
    // the operator's to normalise: its `applied_at` is display-only.
    for t in [
        super::runner::LEDGER_TABLE,
        super::runner::SYSTEM_LEDGER_TABLE,
    ] {
        seen.push((t.to_owned(), "applied_at".to_owned()));
    }

    for entry in inventory::iter::<ModelEntry> {
        for field in entry.schema.fields {
            // `Date` and `Time` are left out on purpose. The problem
            // is the separator between date and time, which only a
            // value holding both has. Sweeping a bare date or time
            // would turn it into a full timestamp.
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

/// Is this error "the thing I was going to sweep is not there yet"?
///
/// Both cases are `SQLite` prepare errors and both are normal during a
/// migration run: `no such table` when the creating migration has not
/// run, `no such column` when the model declares a field whose
/// migration has not run. **Both must be caught here.** The sweep runs
/// at the end of `migrate`, so treating either as fatal aborts a run
/// whose earlier migrations already committed.
///
/// Matched on the message text, because sqlx reports both as a generic
/// database error with no code to key on.
fn is_missing_target(e: &crate::sql::ExecError) -> bool {
    let msg = e.to_string();
    msg.contains("no such table") || msg.contains("no such column")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A duplicate in the framework list means a second `UPDATE` over
    /// the same column on every migrate run.
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

    /// The format the sweep writes must have the shape the sweep
    /// assumes. Asserted against the constant, never a literal copy of
    /// it, or the test passes while the code drifts.
    #[test]
    fn the_canonical_format_has_the_shape_the_sweep_assumes() {
        let f = SQLITE_DATETIME_FORMAT;
        // Derive the rendered width from the format instead of
        // restating it: `%Y` is 4 chars, `%m %d %H %M` are 2 each, and
        // `%f` is `SS.SSS`, 6 chars.
        let rendered_len = f.len() - "%Y".len() + 4 - 4 * ("%m".len() - 2) - "%f".len() + 6;
        assert_eq!(
            rendered_len, 32,
            "the canonical format renders {rendered_len} chars, not 32; the \
             sweep's GLOB shape test and every width assumption downstream \
             are keyed on 32. Format: {f}"
        );
        assert!(
            f.contains("T%H"),
            "the separator must be `T`: a space is the legacy shape that \
             sorts below it and started #1464. Format: {f}"
        );
        assert!(
            f.ends_with("+00:00"),
            "the offset must be explicit and fixed-width, or two encodings \
             of one instant differ. Format: {f}"
        );
    }
}
