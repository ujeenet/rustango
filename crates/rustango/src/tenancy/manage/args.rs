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

/// Refuse arguments a verb does not take.
///
/// Silently ignoring them means a typo'd flag (`list-operators -active`) or a
/// misremembered argument runs the bare verb and exits 0, so the caller thinks
/// it did what they asked (#1356).
pub(super) fn reject_extra_positionals(
    args: &[String],
    allowed: usize,
    verb: &str,
) -> Result<(), TenancyError> {
    let extra: Vec<&String> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .skip(allowed)
        .collect();
    match extra.first() {
        Some(a) => Err(TenancyError::Validation(format!(
            "{verb} does not take `{a}`"
        ))),
        None => Ok(()),
    }
}

/// What a verb accepts. Declared once so every verb refuses the same way:
/// an ignored flag runs the bare verb and exits 0 (#1909, #1910).
pub(super) struct Spec<'a> {
    pub verb: &'a str,
    pub usage: &'a str,
    /// Flags without a value (`--dry-run`).
    pub switches: &'a [&'a str],
    /// Flags that consume the next argument (`--password <p>`).
    pub valued: &'a [&'a str],
    pub max_positionals: usize,
}

/// Arguments split by [`parse`]: flag values are never positionals.
#[derive(Debug)]
pub(super) struct Parsed {
    positionals: Vec<String>,
    switches: Vec<String>,
    values: Vec<(String, String)>,
}

impl Parsed {
    pub(super) fn positional(&self, i: usize) -> Option<&String> {
        self.positionals.get(i)
    }

    pub(super) fn has(&self, flag: &str) -> bool {
        self.switches.iter().any(|s| s == flag)
    }

    /// Every value given for `flag`, in order.
    fn values(&self, flag: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(f, _)| f == flag)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The value for `flag`; refuses it given twice rather than guessing.
    pub(super) fn value(&self, flag: &str) -> Result<Option<&str>, TenancyError> {
        match self.values(flag).as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            _ => Err(TenancyError::Validation(format!("{flag} given twice"))),
        }
    }
}

/// Split `args` per `spec`, refusing unknown flags and extra positionals.
/// `--help` / `-h` return the usage as a validation error, never run the verb.
pub(super) fn parse(args: &[String], spec: &Spec<'_>) -> Result<Parsed, TenancyError> {
    let mut out = Parsed {
        positionals: Vec::new(),
        switches: Vec::new(),
        values: Vec::new(),
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let a = arg.as_str();
        if a == "--help" || a == "-h" {
            return Err(TenancyError::Validation(spec.usage.to_owned()));
        }
        if spec.switches.contains(&a) {
            out.switches.push(a.to_owned());
        } else if spec.valued.contains(&a) {
            out.values.push((a.to_owned(), next_value(&mut iter, a)?));
        } else if a.starts_with('-') {
            return Err(TenancyError::Validation(format!(
                "{}: unknown flag `{a}` — usage: {}",
                spec.verb, spec.usage
            )));
        } else if out.positionals.len() == spec.max_positionals {
            return Err(TenancyError::Validation(format!(
                "{} does not take `{a}` — usage: {}",
                spec.verb, spec.usage
            )));
        } else {
            out.positionals.push(a.to_owned());
        }
    }
    Ok(out)
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

    const SPEC: Spec<'static> = Spec {
        verb: "v",
        usage: "v <a> [--on] [--k <x>]",
        switches: &["--on"],
        valued: &["--k"],
        max_positionals: 1,
    };

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parse_takes_flag_values_out_of_the_positionals() {
        let p = parse(&argv(&["--k", "false", "acme", "--on"]), &SPEC).expect("ok");
        assert_eq!(p.positional(0).map(String::as_str), Some("acme"));
        assert!(p.has("--on"));
        assert_eq!(p.value("--k").unwrap(), Some("false"));
    }

    #[test]
    fn parse_refuses_unknown_flags_extra_positionals_and_help() {
        for bad in [&["--rol"][..], &["a", "b"], &["--help"], &["-x"]] {
            assert!(parse(&argv(bad), &SPEC).is_err(), "{bad:?}");
        }
        assert!(parse(&argv(&["--k", "1", "--k", "2"]), &SPEC)
            .unwrap()
            .value("--k")
            .is_err());
    }

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
