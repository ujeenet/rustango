//! Live-input helpers for the `manage` CLI.
//!
//! Every prompt returns `Ok(None)` when stdin is **not** a TTY —
//! programmatic callers (the multitenant_demo bootstrap, tests,
//! scripts piping into the CLI) keep the existing
//! `Validation("--password required")` error path. Interactive
//! shells fall through to `Some(value)` after asking the user.
//!
//! Why this shape: the existing `manage::run` API takes
//! `args: impl IntoIterator<Item = String>` and returns a result —
//! tests + the demo lean on that contract. Adding a `Prompter` trait
//! parameter would ripple through every callsite. Wrapping live
//! input in TTY-gated helpers preserves the API and makes the
//! interactive UX opt-in via "did stdin come from a terminal?".

use std::io::{self, BufRead, IsTerminal as _, Write as _};

/// One line of input, however the caller obtains it.
///
/// The wizard and the menu prompt between verb calls, and the verbs
/// prompt for themselves through [`ask`]. Holding a `StdinLock` across
/// that boundary deadlocks: the outer loop owns the lock and the reader's
/// buffer while the verb tries to read the same stream underneath it
/// (#1360). So the outer loops take a `LineSource` instead, and the
/// stdin implementation locks per read — leaving the stream free between
/// prompts for whatever they call.
///
/// A `BufRead` is still a `LineSource`, which is what lets tests drive
/// both loops with a `Cursor` and no terminal at all.
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

/// Reads stdin by locking it for that read only.
///
/// The lock is released before the caller does anything else, so a verb
/// invoked between two prompts can take it itself.
pub struct SharedStdin;

impl LineSource for SharedStdin {
    fn read_line_from(&mut self, buf: &mut String) -> io::Result<usize> {
        io::stdin().read_line(buf)
    }
}

/// Read a non-empty trimmed line from stdin. Returns `Ok(None)` if
/// stdin isn't a TTY, the user typed nothing, or EOF was hit. The
/// caller decides whether `None` is fatal or merely "no answer".
///
/// Output is written to **stderr** so prompts don't interleave with
/// machine-readable stdout (e.g. when the operator pipes manage
/// output into another tool).
///
/// # Errors
/// Returns [`io::Error`] for terminal write/read failures.
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
        // EOF — Ctrl+D before any input.
        return Ok(None);
    }
    let trimmed = buf.trim().to_owned();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

/// Like [`ask`] but reads a password without echoing to the
/// terminal. Uses `rpassword` which calls `tcsetattr` on Unix and
/// the equivalent on Windows.
///
/// Returns `Ok(None)` for non-TTY stdin OR empty input — same shape
/// as [`ask`] so callers can use one match arm.
///
/// # Errors
/// Returns [`io::Error`] for terminal-mode toggle or read failures.
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
