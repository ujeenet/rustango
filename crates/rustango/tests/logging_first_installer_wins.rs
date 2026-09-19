//! Backing test for `docs/logging.md` — "the first installer wins".
//!
//! Every installer uses `try_init`, so a second one is a no-op rather
//! than a panic. That is what makes `#[rustango::main]` safe to pair
//! with anything — and it is also why a *later*, more specific
//! configuration is discarded. The page tells readers that, and a
//! scaffolded project ran straight into it: the macro installs before
//! the runtime is built, so a `Cli::with_logging()` inside `main` is
//! always second.
//!
//! `tracing` discards it silently. `Setup::install` no longer does —
//! it reports the discard and names `#[rustango::main(logging =
//! false)]` (#1465). Both halves are asserted below: who wins, and
//! that losing is now audible.
//!
//! Its own test binary with exactly **one** test, because the global
//! subscriber installed here is process-wide and irreversible — the same
//! reason `logging_file_appender_live.rs` is separate. A second test in
//! this file would install first and this one would then measure that
//! test's subscriber instead, which is how the first draft failed.
//!
//! **On measuring this.** The first version of this test asserted on
//! `LevelFilter::current()` and reported that the *second* installer
//! had won. It hadn't: `current()` is tracing's process-global max-level
//! hint, and merely *constructing* an `EnvFilter` raises it — installed
//! or not. The hint answers "could anything want this level", not "whose
//! subscriber is active". Giving each subscriber its own writer answers
//! the question actually being asked: the event can only land in one.

#![cfg(feature = "runtime")]

use std::sync::{Arc, Mutex};

/// Captures what a subscriber wrote.
#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl CaptureWriter {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap_or_default()
    }
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install two subscribers writing to two different buffers, then emit
/// one event. It lands in exactly one of them, and which one is the
/// whole claim.
#[test]
fn the_second_installer_is_discarded_and_setup_says_so() {
    // `RUST_LOG` beats the default filter and the harness inherits the
    // developer's environment. Clear it so this measures installer
    // precedence rather than whatever is exported in the shell.
    std::env::remove_var("RUST_LOG");

    let first = CaptureWriter::default();
    let second = CaptureWriter::default();

    let w = first.clone();
    // `with_ansi(false)`: enabling the `ansi` feature (#1480) made
    // `fmt` colour by default, and escape codes land *between* the
    // characters a substring assertion looks for — `tenant=acme`
    // becomes `\x1b[3mtenant\x1b[0m\x1b[2m=\x1b[0macme`. A test that
    // reads rendered output must ask for plain text.
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || w.clone())
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();

    // Exactly what `Cli::with_logging()` does later in `main`, and what
    // a second `logging::setup()` would do: build a full config and
    // install it. The API gives back no signal that it didn't take.
    let w = second.clone();
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || w.clone())
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();

    tracing::info!("marker_event_for_installer_precedence");

    assert!(
        first
            .contents()
            .contains("marker_event_for_installer_precedence"),
        "the event should reach the FIRST subscriber, got: {:?}",
        first.contents()
    );
    assert!(
        second.contents().is_empty(),
        "the second installer must be inert — if the event reached it, \
         `try_init` started replacing the global subscriber and the \
         page's advice about install order is now wrong. Got: {:?}",
        second.contents()
    );

    // #1465. The framework's own installer is the one a project
    // actually reaches, via `Cli::with_logging()`. It loses the same
    // way — and used to lose in silence, which is why `format =
    // "json"` stayed pretty with nothing to explain it.
    let before = first.contents().len();
    let guard = rustango::logging::Setup::new()
        .with_format(rustango::logging::Format::Json)
        .install();
    assert!(
        guard.is_none(),
        "no file sink was configured, so there is no guard to hand back",
    );
    let warning = &first.contents()[before..];
    assert!(
        warning.contains("[logging] settings ignored"),
        "a discarded `Setup::install` must say so — that silence was the \
         whole of #1465. Emitted since the last assert: {warning:?}",
    );
    assert!(
        warning.contains("logging = false"),
        "the warning has to name the fix, or it only tells the operator \
         they have a problem. Got: {warning:?}",
    );
}
