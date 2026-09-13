//! Interactive `manage wizard` — walks a fresh project from
//! "I just `cargo rustango new`'d" to "I have a tenant + an
//! operator + a tenant superuser, ready to log in".
//!
//! Roadmap #2, v0.30.14. Replaces a 4-5 verb chain a new user
//! has to learn (`init-tenancy` → `migrate-registry` →
//! `create-operator` → `create-tenant` → `create-superuser`)
//! with a single conversational flow.
//!
//! ## Design
//!
//! - Every step is OPT-IN. The wizard prompts `[Y/n]` and skips
//!   when the user answers `n`. New projects can run the whole
//!   thing; partial setups can run just the steps they need.
//! - Reads from a `BufRead` (rather than `std::io::stdin`
//!   directly) so tests can pipe canned input without touching
//!   the terminal.
//! - Writes prompts to the same writer the verb dispatcher uses
//!   so capture works uniformly. (No separate stderr split — a
//!   user piping to a file sees both prompts and verb output;
//!   that's the right shape for an interactive wizard.)
//! - Each step calls the existing internal verb function
//!   directly. No process spawning, no argv reconstruction —
//!   just a Rust function call with the answers the user gave.
//! - Empty-input default: pressing Enter on a `[Y/n]` prompt
//!   accepts the capital letter. Pressing Enter on a value
//!   prompt with a default echoed in `(default: foo)` accepts
//!   that default.
//!
//! ## v1 scope
//!
//! Five steps in order:
//! 1. Scaffold an app (`startapp <name>`)
//! 2. Initialize tenancy (`init-tenancy`)
//! 3. Apply registry migrations (`migrate-registry`)
//! 4. Create an operator (`create-operator`)
//! 5. Create a tenant + first superuser (`create-tenant` +
//!    `create-superuser`)
//!
//! v2 ideas (not in v1): provision a non-superuser tenant user,
//! set up jobs queue, configure CSP / brand. Each is a separate
//! verb today; bundling into the wizard is straightforward when
//! the user asks for it.

use std::io::{BufRead, Write};
use std::path::Path;

use crate::tenancy::TenantPools;

use super::tenants::create_tenant;
use super::users::{create_operator_cmd, create_superuser_cmd};
use super::InitTenancyFn;

/// Top-level entrypoint for `manage wizard`. Drives the prompt
/// flow against `reader` (typically stdin) + `writer` (typically
/// stdout), calling existing verb functions for each step.
///
/// # Errors
/// Any underlying verb's error (DB failures, validation issues)
/// surfaces here. The wizard does NOT swallow errors silently —
/// a failed step aborts the wizard so the user can retry from
/// where they were.
pub(super) async fn wizard_cmd<R: BufRead, W: Write + Send, DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    init_fn: InitTenancyFn,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), super::TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    write_intro(writer)?;

    // Step 1 — scaffold an app
    if prompt_yes_no(reader, writer, "Scaffold a new app?", true)? {
        let name = prompt_value(reader, writer, "  App name", Some("blog"))?;
        if !name.is_empty() {
            // startapp lives in the migrate-side scaffold module.
            // Call it directly so the wizard works regardless of
            // which dispatcher the user invoked us through.
            let opts = crate::migrate::scaffold::StartAppOptions {
                app_name: name.clone(),
                manage_bin: None,
                base_dir: None,
                crate_root: None,
            };
            let report = crate::migrate::scaffold::startapp(Path::new("."), &opts)
                .map_err(|e| super::TenancyError::Validation(format!("startapp `{name}`: {e}")))?;
            for path in &report.written {
                writeln!(writer, "  wrote {path}")?;
            }
            for path in &report.skipped {
                writeln!(writer, "  skipped (exists) {path}")?;
            }
            for hint in &report.manual_steps {
                writeln!(writer, "  manual step: {hint}")?;
            }
        }
    }

    // Step 2 — initialize tenancy bootstrap migrations
    if prompt_yes_no(reader, writer, "Initialize tenancy?", true)? {
        super::migrations::init_tenancy_cmd_with(dir, writer, init_fn)?;
    }

    // Step 3 — apply registry migrations (creates rustango_orgs etc.)
    if prompt_yes_no(reader, writer, "Apply registry migrations now?", true)? {
        super::migrations::migrate_registry_cmd(pools, dir, writer).await?;
    }

    // Step 4 — operator (logs into the operator console)
    if prompt_yes_no(reader, writer, "Create an operator account?", true)? {
        let username = prompt_value(reader, writer, "  Operator username", Some("admin"))?;
        let password = prompt_value(reader, writer, "  Operator password", None)?;
        if username.is_empty() || password.is_empty() {
            writeln!(writer, "  (skipped — username and password required)")?;
        } else {
            create_operator_cmd(
                pools,
                &[username.clone(), "--password".into(), password.clone()],
                writer,
            )
            .await?;
        }
    }

    // Step 5 — tenant + its first superuser. Bundled together
    // because a tenant with no users is unusable.
    if prompt_yes_no(reader, writer, "Create a tenant?", true)? {
        let slug = prompt_value(reader, writer, "  Tenant slug", Some("acme"))?;
        let display = prompt_value(reader, writer, "  Display name", Some(&slug))?;
        if slug.is_empty() {
            writeln!(writer, "  (skipped — slug required)")?;
        } else {
            create_tenant(
                pools,
                registry_url,
                dir,
                &[slug.clone(), "--display-name".into(), display.clone()],
                writer,
            )
            .await?;

            if prompt_yes_no(
                reader,
                writer,
                "  Create a superuser for this tenant?",
                true,
            )? {
                let username =
                    prompt_value(reader, writer, "    Superuser username", Some("admin"))?;
                let password = prompt_value(reader, writer, "    Superuser password", None)?;
                if username.is_empty() || password.is_empty() {
                    writeln!(writer, "    (skipped — username and password required)")?;
                } else {
                    create_superuser_cmd(
                        pools,
                        registry_url,
                        &[
                            slug.clone(),
                            username.clone(),
                            "--password".into(),
                            password.clone(),
                        ],
                        writer,
                    )
                    .await?;
                }
            }
        }
    }

    write_outro(writer)?;
    Ok(())
}

fn write_intro<W: Write>(w: &mut W) -> std::io::Result<()> {
    writeln!(
        w,
        "rustango wizard — interactive setup\n\
         ===================================\n\
         Press Enter to accept the default, or type your own value. Each\n\
         step asks before running; type `n` to skip.\n"
    )
}

fn write_outro<W: Write>(w: &mut W) -> std::io::Result<()> {
    writeln!(
        w,
        "\nWizard complete. Next:\n  • cargo run                   (boot the server)\n  \
         • visit /__login              (operator console)\n  \
         • visit <slug>.localhost      (tenant admin)"
    )
}

/// Prompt the user with a yes/no question. `default_yes` controls
/// which letter is capitalized in the `[Y/n]` / `[y/N]` hint and
/// which way an empty answer (just Enter) defaults.
///
/// Accepts: `y`, `Y`, `yes`, `YES`, `1`, `true` → yes; everything
/// else → no. (Case-insensitive.)
pub(super) fn prompt_yes_no<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    question: &str,
    default_yes: bool,
) -> std::io::Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    write!(writer, "{question} {hint} ")?;
    writer.flush()?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "y" | "yes" | "1" | "true"
    ))
}

/// Prompt the user for a string value. When `default` is `Some`,
/// pressing Enter on an empty input returns the default. When
/// `None`, an empty input returns an empty string (the caller
/// decides whether to treat that as "skip").
///
/// Returns the trimmed input (no leading/trailing whitespace) so
/// downstream verbs don't see accidental newlines.
pub(super) fn prompt_value<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: Option<&str>,
) -> std::io::Result<String> {
    match default {
        Some(d) => write!(writer, "{label} (default: {d}): ")?,
        None => write!(writer, "{label}: ")?,
    }
    writer.flush()?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(default.unwrap_or("").to_owned());
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// `prompt_yes_no` accepts every truthy spelling, defaults
    /// correctly on empty input.
    #[test]
    fn prompt_yes_no_accepts_truthy_strings_and_defaults_on_empty() {
        let cases = [
            // (input, default_yes, expected)
            ("y\n", false, true),
            ("Y\n", false, true),
            ("yes\n", false, true),
            ("YES\n", false, true),
            ("1\n", false, true),
            ("true\n", false, true),
            ("n\n", true, false),
            ("N\n", true, false),
            ("no\n", true, false),
            ("anything-else\n", true, false),
            // Empty → use the default.
            ("\n", true, true),
            ("\n", false, false),
        ];
        for (input, default_yes, expected) in cases {
            let mut r = Cursor::new(input.as_bytes().to_vec());
            let mut w: Vec<u8> = Vec::new();
            let got = prompt_yes_no(&mut r, &mut w, "?", default_yes).unwrap();
            assert_eq!(got, expected, "input={input:?} default_yes={default_yes}");
        }
    }

    /// `prompt_yes_no` writes the prompt + the right `[Y/n]` /
    /// `[y/N]` hint based on the default — important for users
    /// who scan the prompt to know which is the safe choice.
    #[test]
    fn prompt_yes_no_writes_correct_hint_letter_capitalized() {
        let mut r = Cursor::new(b"y\n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        prompt_yes_no(&mut r, &mut w, "Apply registry migrations?", true).unwrap();
        let out = String::from_utf8(w).unwrap();
        assert!(out.contains("[Y/n]"), "got: {out}");
        assert!(!out.contains("[y/N]"));

        let mut r = Cursor::new(b"y\n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        prompt_yes_no(&mut r, &mut w, "Wipe everything?", false).unwrap();
        let out = String::from_utf8(w).unwrap();
        assert!(out.contains("[y/N]"), "got: {out}");
        assert!(!out.contains("[Y/n]"));
    }

    /// `prompt_value` returns the typed value, trimmed; empty
    /// input + default returns the default; empty input + no
    /// default returns "".
    #[test]
    fn prompt_value_trims_input_and_falls_back_to_default() {
        // Typed value (with trailing whitespace).
        let mut r = Cursor::new(b"  blog  \n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        assert_eq!(
            prompt_value(&mut r, &mut w, "App name", Some("default_app")).unwrap(),
            "blog"
        );

        // Empty input → default kicks in.
        let mut r = Cursor::new(b"\n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        assert_eq!(
            prompt_value(&mut r, &mut w, "App name", Some("default_app")).unwrap(),
            "default_app"
        );

        // Empty input + no default → empty string (skip marker).
        let mut r = Cursor::new(b"\n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        assert_eq!(prompt_value(&mut r, &mut w, "Password", None).unwrap(), "");
    }

    /// Default value is echoed in the prompt so users know what
    /// they'll get if they press Enter.
    #[test]
    fn prompt_value_shows_default_in_prompt() {
        let mut r = Cursor::new(b"\n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        prompt_value(&mut r, &mut w, "App name", Some("blog")).unwrap();
        let out = String::from_utf8(w).unwrap();
        assert!(out.contains("default: blog"), "got: {out}");

        let mut r = Cursor::new(b"\n".to_vec());
        let mut w: Vec<u8> = Vec::new();
        prompt_value(&mut r, &mut w, "Password", None).unwrap();
        let out = String::from_utf8(w).unwrap();
        assert!(!out.contains("default:"), "got: {out}");
    }
}
