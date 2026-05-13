//! Helpers shared across the verb modules: positional / flag-value
//! consumption, and SQL identifier quoting.

use crate::tenancy::error::TenancyError;

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
#[cfg(feature = "postgres")]
pub(super) fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Reject `args.first()` if it looks like a flag (starts with `-`).
/// Used by every verb that takes a positional `<slug>` (or `<username>`)
/// as its first argument — without this check, `cargo run -- <verb>
/// --help` is silently parsed as `<verb> "--help"` and ends up
/// creating a row literally named `--help` (#79). Returns a
/// validation error with usage hint for the help case, or a clear
/// "expected positional <name> first, got flag `…`" error for any
/// other leading flag.
///
/// Returns `Ok(())` when:
///   - `args` is empty (verb may prompt interactively)
///   - `args.first()` is a non-flag token (a real positional)
pub(super) fn reject_leading_flag(
    args: &[String],
    verb: &str,
    arg_name: &str,
    usage: &str,
) -> Result<(), TenancyError> {
    let Some(first) = args.first() else {
        return Ok(());
    };
    if !first.starts_with('-') {
        return Ok(());
    }
    if first == "--help" || first == "-h" {
        return Err(TenancyError::Validation(usage.to_owned()));
    }
    Err(TenancyError::Validation(format!(
        "{verb}: expected positional <{arg_name}> first, got flag `{first}`"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_leading_flag_passes_normal_positional() {
        let args = vec!["acme".to_owned()];
        assert!(reject_leading_flag(&args, "create-tenant", "slug", "USAGE").is_ok());
    }

    #[test]
    fn reject_leading_flag_passes_empty() {
        // Verb may prompt interactively when no args at all.
        assert!(reject_leading_flag(&[], "create-tenant", "slug", "USAGE").is_ok());
    }

    #[test]
    fn reject_leading_flag_emits_help_for_help_flag() {
        let args = vec!["--help".to_owned()];
        let err =
            reject_leading_flag(&args, "create-tenant", "slug", "USAGE\nFLAGS\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("USAGE"), "expected usage in `{msg}`");
    }

    #[test]
    fn reject_leading_flag_emits_help_for_short_help_flag() {
        let args = vec!["-h".to_owned()];
        assert!(reject_leading_flag(&args, "create-tenant", "slug", "USAGE").is_err());
    }

    #[test]
    fn reject_leading_flag_rejects_unknown_flag_with_clear_message() {
        let args = vec!["--mode".to_owned(), "schema".to_owned()];
        let err = reject_leading_flag(&args, "create-tenant", "slug", "USAGE").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("expected positional <slug>"), "{msg}");
        assert!(msg.contains("--mode"), "{msg}");
    }
}
