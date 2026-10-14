//! Prompts for the `manage` CLI.
//!
//! Every prompt returns `Ok(None)` when stdin is not a terminal, so
//! scripts and tests still get the usual "argument required" error
//! instead of blocking. In a real shell the prompt runs and returns
//! `Some(value)`. This keeps the `manage::run` signature unchanged
//! and makes the interactive path opt in by itself.

use std::io::{self, BufRead, IsTerminal as _, Write as _};

/// One line of input, from wherever the caller reads it.
///
/// The wizard and the menu prompt between commands, and a command
/// may prompt again through [`ask`]. Holding a `StdinLock` across
/// that boundary deadlocks, so the loops take a `LineSource` and
/// the stdin version locks for one read at a time.
///
/// Any `BufRead` is a `LineSource`, so tests can drive the loops
/// from a `Cursor` with no terminal.
pub trait LineSource {
    /// Read one line, including its newline. `Ok(0)` means EOF.
    ///
    /// # Errors
    /// Whatever the underlying reader returns.
    fn read_line_from(&mut self, buf: &mut String) -> io::Result<usize>;
}

impl<R: BufRead> LineSource for R {
    fn read_line_from(&mut self, buf: &mut String) -> io::Result<usize> {
        self.read_line(buf)
    }
}

/// Reads stdin, locking it for one read only. The lock is free
/// again before the caller does anything else.
pub struct SharedStdin;

impl LineSource for SharedStdin {
    fn read_line_from(&mut self, buf: &mut String) -> io::Result<usize> {
        io::stdin().read_line(buf)
    }
}

/// Read one trimmed, non-empty line from stdin. `Ok(None)` means
/// stdin is not a terminal, the user typed nothing, or we hit EOF;
/// the caller decides if that is an error.
///
/// The prompt goes to stderr, so it does not mix into stdout when
/// someone pipes the output elsewhere.
///
/// # Errors
/// [`io::Error`] if the terminal read or write fails.
pub fn ask(prompt: &str) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut stderr = io::stderr().lock();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    drop(stderr);
    let mut buf = String::new();
    let n = io::stdin().read_line(&mut buf)?;
    if n == 0 {
        // Ctrl+D before any input.
        return Ok(None);
    }
    let trimmed = buf.trim().to_owned();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

/// Like [`ask`], but the typed characters are not shown on screen.
/// `rpassword` turns echo off for the read.
///
/// `Ok(None)` for a non-terminal stdin or empty input, the same as
/// [`ask`].
///
/// # Errors
/// [`io::Error`] if switching terminal mode or reading fails.
pub fn ask_password(prompt: &str) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    Ok(password_answer(rpassword::prompt_password(prompt)?))
}

/// Kept byte for byte: spaces are part of a password, and `--password`
/// never trimmed, so a trimmed prompt stored a different secret (#1910).
fn password_answer(pw: String) -> Option<String> {
    (!pw.is_empty()).then_some(pw)
}

/// Env var a `manage` verb reads a new password from off a terminal.
pub const PASSWORD_ENV: &str = "RUSTANGO_PASSWORD";

/// Where a `manage` verb reads a new password from (#2589).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PasswordInput {
    /// `--password <p>`; warns when it sits in the process argv.
    Argv(String),
    /// `--password-stdin`: one line from stdin.
    Stdin,
    /// [`PASSWORD_ENV`], else a terminal prompt asked twice.
    EnvOrPrompt,
}

impl PasswordInput {
    /// From the `--password` value and the `--password-stdin` switch.
    ///
    /// # Errors
    /// Both flags given.
    pub fn from_flags(password: Option<String>, stdin: bool) -> Result<Self, NewPasswordError> {
        match (password, stdin) {
            (Some(_), true) => Err(NewPasswordError::Conflict),
            (Some(p), false) => Ok(Self::Argv(p)),
            (None, true) => Ok(Self::Stdin),
            (None, false) => Ok(Self::EnvOrPrompt),
        }
    }
}

/// Why [`read_new_password`] gave no password.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NewPasswordError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("--password and --password-stdin are mutually exclusive")]
    Conflict,
    #[error("no password: pass --password-stdin, set RUSTANGO_PASSWORD, or run in a terminal")]
    Missing,
    #[error("passwords didn't match")]
    Mismatch,
    #[error("{0} (pass --skip-validation to bypass)")]
    Invalid(String),
}

/// The terminal I/O [`read_new_password`] needs, so tests can script it.
trait PasswordConsole {
    fn is_tty(&self) -> bool;
    fn env(&self, name: &str) -> Option<String>;
    fn in_argv(&self, value: &str) -> bool;
    fn stdin_line(&mut self) -> io::Result<String>;
    fn ask_hidden(&mut self, prompt: &str) -> io::Result<Option<String>>;
    fn ask_line(&mut self, prompt: &str) -> io::Result<Option<String>>;
    fn warn(&mut self, msg: &str);
}

struct Terminal;

impl PasswordConsole for Terminal {
    fn is_tty(&self) -> bool {
        io::stdin().is_terminal()
    }
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn in_argv(&self, value: &str) -> bool {
        std::env::args().any(|a| a == value)
    }
    fn stdin_line(&mut self) -> io::Result<String> {
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        Ok(buf)
    }
    fn ask_hidden(&mut self, prompt: &str) -> io::Result<Option<String>> {
        ask_password(prompt)
    }
    fn ask_line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        ask(prompt)
    }
    fn warn(&mut self, msg: &str) {
        eprintln!("{msg}");
    }
}

/// Read a new password for a `manage` verb and run the form validators
/// on it, like `createsuperuser`. A terminal user may bypass a failed
/// check; scripts pass `skip_validation`.
///
/// # Errors
/// [`NewPasswordError`].
pub fn read_new_password(
    input: PasswordInput,
    skip_validation: bool,
    prompt: &str,
) -> Result<String, NewPasswordError> {
    read_new_password_on(&mut Terminal, input, skip_validation, prompt)
}

fn read_new_password_on(
    console: &mut dyn PasswordConsole,
    input: PasswordInput,
    skip_validation: bool,
    prompt: &str,
) -> Result<String, NewPasswordError> {
    let (password, interactive) = match input {
        PasswordInput::Argv(p) => {
            if console.in_argv(&p) {
                console.warn(
                    "warning: --password is visible in `ps` and shell history; \
                     use --password-stdin or RUSTANGO_PASSWORD",
                );
            }
            (p, false)
        }
        PasswordInput::Stdin => {
            let line = console.stdin_line()?;
            let p = line.strip_suffix('\n').unwrap_or(&line);
            let p = p.strip_suffix('\r').unwrap_or(p);
            (
                password_answer(p.to_owned()).ok_or(NewPasswordError::Missing)?,
                false,
            )
        }
        PasswordInput::EnvOrPrompt => match console.env(PASSWORD_ENV).and_then(password_answer) {
            Some(p) => (p, false),
            None if console.is_tty() => {
                let p = console
                    .ask_hidden(prompt)?
                    .ok_or(NewPasswordError::Missing)?;
                if console.ask_hidden("Password (again): ")?.as_deref() != Some(p.as_str()) {
                    return Err(NewPasswordError::Mismatch);
                }
                (p, true)
            }
            None => return Err(NewPasswordError::Missing),
        },
    };
    if skip_validation {
        return Ok(password);
    }
    if let Err(e) = crate::password_validators::check_builtin_form_password(&password) {
        if interactive {
            console.warn(&format!("error: {}", e.message));
            let answer = console
                .ask_line("Bypass password validation and create user anyway? [y/N]: ")?
                .unwrap_or_default();
            if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
                return Ok(password);
            }
        }
        return Err(NewPasswordError::Invalid(e.message));
    }
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Script {
        tty: bool,
        env: Option<String>,
        argv: Vec<String>,
        stdin: String,
        answers: Vec<&'static str>,
        warnings: Vec<String>,
    }

    impl PasswordConsole for Script {
        fn is_tty(&self) -> bool {
            self.tty
        }
        fn env(&self, _: &str) -> Option<String> {
            self.env.clone()
        }
        fn in_argv(&self, value: &str) -> bool {
            self.argv.iter().any(|a| a == value)
        }
        fn stdin_line(&mut self) -> io::Result<String> {
            Ok(std::mem::take(&mut self.stdin))
        }
        fn ask_hidden(&mut self, _: &str) -> io::Result<Option<String>> {
            Ok(Some(self.answers.remove(0).to_owned()).filter(|a| !a.is_empty()))
        }
        fn ask_line(&mut self, p: &str) -> io::Result<Option<String>> {
            self.ask_hidden(p)
        }
        fn warn(&mut self, msg: &str) {
            self.warnings.push(msg.to_owned());
        }
    }

    fn read(c: &mut Script, input: PasswordInput, skip: bool) -> Result<String, NewPasswordError> {
        read_new_password_on(c, input, skip, "Password: ")
    }

    /// #2589 — every source runs the form validators.
    #[test]
    fn a_short_password_is_refused_from_every_source() {
        let short = || PasswordInput::Argv("pw".into());
        assert!(matches!(
            read(&mut Script::default(), short(), false),
            Err(NewPasswordError::Invalid(_))
        ));
        let mut c = Script {
            stdin: "pw\n".into(),
            ..Script::default()
        };
        assert!(matches!(
            read(&mut c, PasswordInput::Stdin, false),
            Err(NewPasswordError::Invalid(_))
        ));
        let mut c = Script {
            env: Some("pw".into()),
            ..Script::default()
        };
        assert!(matches!(
            read(&mut c, PasswordInput::EnvOrPrompt, false),
            Err(NewPasswordError::Invalid(_))
        ));
        assert_eq!(read(&mut Script::default(), short(), true).unwrap(), "pw");
    }

    #[test]
    fn the_prompt_asks_twice_and_offers_a_bypass() {
        let mut c = Script {
            tty: true,
            answers: vec!["long-enough-1", "long-enough-2"],
            ..Script::default()
        };
        assert!(matches!(
            read(&mut c, PasswordInput::EnvOrPrompt, false),
            Err(NewPasswordError::Mismatch)
        ));
        let mut c = Script {
            tty: true,
            answers: vec!["pw", "pw", "y"],
            ..Script::default()
        };
        assert_eq!(
            read(&mut c, PasswordInput::EnvOrPrompt, false).unwrap(),
            "pw"
        );
        let mut c = Script {
            tty: true,
            answers: vec!["pw", "pw", ""],
            ..Script::default()
        };
        assert!(matches!(
            read(&mut c, PasswordInput::EnvOrPrompt, false),
            Err(NewPasswordError::Invalid(_))
        ));
    }

    #[test]
    fn off_a_terminal_the_password_comes_from_env_or_stdin() {
        assert!(matches!(
            read(&mut Script::default(), PasswordInput::EnvOrPrompt, false),
            Err(NewPasswordError::Missing)
        ));
        let mut c = Script {
            env: Some("from-env-123".into()),
            ..Script::default()
        };
        assert_eq!(
            read(&mut c, PasswordInput::EnvOrPrompt, false).unwrap(),
            "from-env-123"
        );
        let mut c = Script {
            stdin: " spaced pass \r\n".into(),
            ..Script::default()
        };
        assert_eq!(
            read(&mut c, PasswordInput::Stdin, false).unwrap(),
            " spaced pass "
        );
        assert!(matches!(
            PasswordInput::from_flags(Some("x".into()), true),
            Err(NewPasswordError::Conflict)
        ));
    }

    #[test]
    fn a_password_in_argv_warns() {
        let mut c = Script {
            argv: vec!["long-enough-1".into()],
            ..Script::default()
        };
        read(&mut c, PasswordInput::Argv("long-enough-1".into()), false).unwrap();
        assert_eq!(c.warnings.len(), 1);
        let mut c = Script::default();
        read(&mut c, PasswordInput::Argv("long-enough-1".into()), false).unwrap();
        assert!(c.warnings.is_empty(), "an in-process call is not in ps");
    }

    #[test]
    fn a_prompted_password_keeps_its_spaces() {
        assert_eq!(
            super::password_answer(" pw ".to_owned()).as_deref(),
            Some(" pw ")
        );
        assert_eq!(super::password_answer(String::new()), None);
    }
}
