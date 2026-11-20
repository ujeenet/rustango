//! `sendtestemail`.

use super::*;

/// Parsed `manage sendtestemail` arguments.
///
/// Only the `feature = "config"` build of `sendtestemail_cmd` uses
/// this; the `not(feature = "config")` stub short-circuits with a
/// friendly error, so the parser is unreachable there.
#[cfg(all(feature = "config", feature = "email"))]
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SendTestEmailArgs {
    pub(super) to: Option<String>,
    pub(super) from: Option<String>,
    pub(super) subject: Option<String>,
    pub(super) help: bool,
}

#[cfg(all(feature = "config", feature = "email"))]
pub(super) fn parse_sendtestemail_args(args: &[String]) -> Result<SendTestEmailArgs, MigrateError> {
    let mut out = SendTestEmailArgs::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--help" | "-h" => out.help = true,
            "--to" => {
                let v = iter.next().ok_or_else(|| missing_value("--to"))?;
                out.to = Some(v.clone());
            }
            "--from" => {
                let v = iter.next().ok_or_else(|| missing_value("--from"))?;
                out.from = Some(v.clone());
            }
            "--subject" => {
                let v = iter.next().ok_or_else(|| missing_value("--subject"))?;
                out.subject = Some(v.clone());
            }
            other if other.starts_with("--") => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(unexpected_positional(other));
            }
        }
    }
    Ok(out)
}

/// `manage sendtestemail --to <addr>` — send a fixed test email
/// through the mail backend configured in `[mail]` settings. Verifies
/// SMTP credentials and mail wiring without writing a throwaway
/// program.
///
/// Requires the `config` feature so settings can be loaded. Without
/// `--to`, errors with a usage hint. `--from` defaults to the
/// `[mail].from_address` setting (and errors if neither is set).
/// `--subject` defaults to `"rustango: test email"`.
///
/// Successful send prints `sendtestemail: ok` plus the resolved
/// backend name. Send failures surface the underlying `MailError`
/// as a [`MigrateError::Validation`].
#[cfg(all(feature = "config", feature = "email"))]
pub(super) async fn sendtestemail_cmd<W: Write>(
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_sendtestemail_args(args)?;
    if parsed.help {
        writeln!(
            w,
            "sendtestemail --to <addr> [--from <addr>] [--subject <text>]"
        )?;
        writeln!(w)?;
        writeln!(
            w,
            "  Send a test message through the [mail] backend (console / memory /"
        )?;
        writeln!(
            w,
            "  null / smtp). Use to verify SMTP credentials or mail wiring."
        )?;
        writeln!(w)?;
        writeln!(w, "  --to       Recipient address (REQUIRED).")?;
        writeln!(
            w,
            "  --from     From address. Defaults to [mail].from_address."
        )?;
        writeln!(
            w,
            "  --subject  Subject line. Defaults to `rustango: test email`."
        )?;
        return Ok(());
    }

    let to = parsed.to.as_deref().ok_or_else(|| {
        MigrateError::Validation(
            "sendtestemail: --to <addr> is required (run with --help for usage)".to_owned(),
        )
    })?;

    let settings = crate::config::Settings::load_from_env().map_err(|e| {
        MigrateError::Validation(format!(
            "sendtestemail: failed to load settings: {e} — check config/{{default,$tier}}.toml",
        ))
    })?;

    let from = parsed
        .from
        .clone()
        .or_else(|| settings.mail.from_address.clone())
        .ok_or_else(|| {
            MigrateError::Validation(
                "sendtestemail: no --from supplied and [mail].from_address is unset".to_owned(),
            )
        })?;

    let subject = parsed
        .subject
        .clone()
        .unwrap_or_else(|| "rustango: test email".to_owned());
    let body = "If you're reading this in a real inbox, your mail backend is wired correctly.\n\n\
         Sent by `manage sendtestemail`."
        .to_owned();

    let mailer = crate::email::from_settings(&settings.mail)
        .map_err(|e| MigrateError::Validation(format!("sendtestemail: {e}")))?;
    let backend = settings.mail.backend.as_deref().unwrap_or("console");

    let email = crate::email::Email::new()
        .to(to)
        .from(&from)
        .subject(subject)
        .body(body);

    match mailer.send(&email).await {
        Ok(()) => {
            writeln!(
                w,
                "sendtestemail: ok (backend = {backend}, to = {to}, from = {from})"
            )?;
            Ok(())
        }
        Err(e) => Err(MigrateError::Validation(format!(
            "sendtestemail: mailer.send failed (backend = {backend}): {e}"
        ))),
    }
}

#[cfg(not(feature = "config"))]
pub(super) async fn sendtestemail_cmd<W: Write>(
    _args: &[String],
    _w: &mut W,
) -> Result<(), MigrateError> {
    Err(MigrateError::Validation(
        "sendtestemail: this build was compiled without the `config` feature — settings \
         can't be loaded. Rebuild with `--features config`."
            .to_owned(),
    ))
}

#[cfg(all(feature = "config", not(feature = "email")))]
pub(super) async fn sendtestemail_cmd<W: Write>(
    _args: &[String],
    _w: &mut W,
) -> Result<(), MigrateError> {
    Err(MigrateError::Validation(
        "sendtestemail: this build was compiled without the `email` feature — \
         the mail backend isn't linked. Rebuild with `--features email`."
            .to_owned(),
    ))
}
