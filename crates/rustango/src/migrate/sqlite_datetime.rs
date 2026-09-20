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
//! - it rewrites a value only when `strftime` can parse it **and** it
//!   does not already match [`crate::sql::SQLITE_CANONICAL_GLOB`], so a
//!   converted row stops matching and a second run updates nothing;
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
use crate::sql::{Pool, SQLITE_CANONICAL_GLOB, SQLITE_DATETIME_FORMAT};

use super::MigrateError;

/// Framework-owned tables whose `SQLite` DDL is hand-written rather than
/// derived from a `ModelSchema`, so the registry sweep cannot see them.
///
/// `(table, column)`. Kept explicit rather than discovered: these are
/// the tables whose `CREATE TABLE` text this crate ships, so a new one
/// is a change to this crate and belongs in this list by the same edit.
const FRAMEWORK_COLUMNS: &[(&str, &str)] = &[
    // `rustango_translations` is the reason this list exists.
    // `Translation` IS `#[derive(Model)]`, but it declares no DateTime
    // field — `created_at` / `updated_at` live only in the
    // hand-written DDL — so the registry sweep cannot see them.
    ("rustango_translations", "created_at"),
    ("rustango_translations", "updated_at"),
    // `rustango_jobs` is likewise hand-written DDL with no model.
    ("rustango_jobs", "run_at"),
    ("rustango_jobs", "locked_at"),
    ("rustango_jobs", "created_at"),
];

/// Outcome of one sweep, so the caller can say what happened rather
/// than finishing silently.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Normalised {
    /// Rows rewritten, summed over every column visited.
    pub rows: u64,
    /// `table.column` for each column that had at least one legacy row.
    pub columns: Vec<String>,
    /// Targets skipped because the table or the column does not exist
    /// yet. Normal on a fresh database, and normal between deploying a
    /// model with a new `DateTime` field and applying its migration —
    /// the registry lists the column as soon as the binary is built.
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
/// **table or column** is not an error — it is counted in
/// [`Normalised::missing`]. Both are ordinary mid-migration: a fresh
/// database has no tables yet, and a model that declares a new
/// `DateTime` field puts that column in the target list the moment the
/// binary is built, before the migration adding it has run.
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
        // "Already canonical" is a question about **shape**, and it must
        // not be answered by round-tripping through `strftime`.
        //
        // The version before this asked `col <> strftime(FMT, col)`,
        // which reads as "not already canonical" and is not: SQLite's
        // `%f` is *milliseconds* while `encode_datetime`'s chrono
        // `%.6f` is *microseconds*, so `strftime` is not the identity
        // on a value the fixed bind path wrote. `.413681` came back
        // `.414000`. Roughly 999 in 1000 correct rows therefore matched,
        // and every `migrate` rewrote them — rounding each up to 500 µs
        // forward and reporting them as legacy conversions (#1616 rework
        // review, correctness-002 / dialects-001).
        //
        // `GLOB` asks the question directly. `?` is one character and
        // `[0-9]` a digit, so this matches the canonical spelling and
        // nothing else, leaves microsecond precision alone, and is still
        // idempotent — a converted row no longer matches.
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

    // The two migration ledgers, named from the constants that own
    // those names rather than retyped. The first version of this list
    // spelled one of them `rustango_migrations`, which is not a table
    // anywhere in the crate — so it always raised `no such table`, was
    // swallowed into `missing`, and the ledger went unswept although
    // this release changed its DDL.
    //
    // `ensure_ledger_pool_with_ledger` lets an operator rename the
    // ledger (#146), so even the constants only cover the default. A
    // renamed ledger is the operator's to normalise; its `applied_at`
    // is display-only and compared against nothing.
    for t in [
        super::runner::LEDGER_TABLE,
        super::runner::SYSTEM_LEDGER_TABLE,
    ] {
        seen.push((t.to_owned(), "applied_at".to_owned()));
    }

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

/// Is this error "the thing I was going to sweep is not there yet"?
///
/// Both shapes are `SQLite` **prepare** errors and both are ordinary
/// during a migration run:
///
/// ```text
/// no such table: gone        — the migration creating it has not run
/// no such column: nope       — the model declares it, the migration has not run
/// ```
///
/// Only the table case was matched. A model declaring a new `DateTime`
/// column put that column in `targets()` the moment the binary was
/// built, so between deploying the code and applying its migration the
/// sweep raised `no such column`, which fell through to `Err` — and the
/// sweep runs at the *end* of `migrate`, so the run aborted after
/// earlier migrations had already committed (#1616 review,
/// correctness-002; carried unfixed into the rework, where four peers
/// handed it over again).
///
/// Matched on the message because sqlx surfaces both as a generic
/// database error with no code to key on.
fn is_missing_target(e: &crate::sql::ExecError) -> bool {
    let msg = e.to_string();
    msg.contains("no such table") || msg.contains("no such column")
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

    /// The format the sweep writes must have the shape the sweep
    /// assumes — asserted against the **constant**, not a literal.
    ///
    /// The version this replaces declared
    /// `let corrected = "2026-09-19T19:44:55.869000+00:00"` in its own
    /// body and asserted that string's length and tenth byte. It read
    /// no crate code, so it would have passed in an empty repository
    /// and could not fail under any change whatsoever (#1616 rework
    /// review, tests-006). It was itself written to replace an earlier
    /// test judged too weak, which is worth remembering: "assert
    /// something about the format" is not the same as "assert the
    /// format the code uses".
    #[test]
    fn the_canonical_format_has_the_shape_the_sweep_assumes() {
        let f = SQLITE_DATETIME_FORMAT;
        // Rendered width is what the predicate's shape test keys on, so
        // derive it from the format rather than restating it: `%Y` is 4
        // characters, `%m %d %H %M` are 2 each, and `%f` is `SS.SSS`,
        // 6 characters.
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
