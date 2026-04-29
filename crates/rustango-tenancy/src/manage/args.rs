//! Helpers shared across the verb modules: positional / flag-value
//! consumption, and SQL identifier quoting.

use crate::error::TenancyError;

/// Consume the next argument from `iter` as the value for `flag`.
/// Yields a uniform `Validation` error when the flag was passed with
/// no following value.
pub(super) fn next_value<'a, I: Iterator<Item = &'a String>>(
    iter: &mut I,
    flag: &str,
) -> Result<String, TenancyError> {
    iter.next()
        .cloned()
        .ok_or_else(|| TenancyError::Validation(format!("{flag} requires a value")))
}

/// Quote a SQL identifier (table / schema / database name). Doubles
/// any embedded `"` so the quoted form survives unmodified.
pub(super) fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
