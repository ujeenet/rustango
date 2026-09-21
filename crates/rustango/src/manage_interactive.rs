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
    let pw = rpassword::prompt_password(prompt)?;
    let trimmed = pw.trim().to_owned();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}
