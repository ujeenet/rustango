#![cfg(feature = "sqlite")]
//! `.select_for_update()` / `.skip_locked()` / `.nowait()` on SQLite —
//! issue #290 / T2.9. Pins that the writer emits a `tracing::warn!`
//! when a queryset with a `LockMode` compiles against SQLite, and
//! that `.silent_on_sqlite()` suppresses it.
//!
//! Uses `tracing::subscriber::with_default` + a `tracing-subscriber`
//! buffered writer to capture warning output. Skips silently if
//! `tracing-subscriber` isn't reachable (it's a transitive dep via
//! the `runtime` feature, present in all-features CI).

use std::sync::{Arc, Mutex};

use rustango::query::QuerySet;
use rustango::sql::{Dialect, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "lock_warn_job")]
#[allow(dead_code)]
pub struct Job {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 20)]
    status: String,
}

/// Captures `tracing` events into a shared buffer for assertion.
#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

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

fn compile_with_capture<F: FnOnce()>(f: F) -> String {
    let buf: CaptureWriter = CaptureWriter::default();
    let buf_clone = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || buf_clone.clone())
        .with_max_level(tracing::Level::WARN)
        .with_target(true)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = buf.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap_or_default()
}

#[test]
fn select_for_update_against_sqlite_emits_warning() {
    let captured = compile_with_capture(|| {
        let qs = QuerySet::<Job>::default().select_for_update();
        let q = qs.compile().unwrap();
        // Compiling the SQL is what triggers the warning (via
        // write_lock_clause inside the writer).
        let _stmt = Sqlite.compile_select(&q).unwrap();
    });
    assert!(
        captured.contains("select_for_update modifier dropped"),
        "expected SQLite-no-locking warning in captured output, got:\n{captured}"
    );
    assert!(
        captured.contains("rustango::sql::lock"),
        "expected `rustango::sql::lock` target in captured output, got:\n{captured}"
    );
}

#[test]
fn skip_locked_warning_includes_modifier_fields() {
    let captured = compile_with_capture(|| {
        let qs = QuerySet::<Job>::default().select_for_update().skip_locked();
        let q = qs.compile().unwrap();
        let _stmt = Sqlite.compile_select(&q).unwrap();
    });
    assert!(captured.contains("skip_locked=true"));
}

#[test]
fn silent_on_sqlite_suppresses_warning() {
    let captured = compile_with_capture(|| {
        let qs = QuerySet::<Job>::default()
            .select_for_update()
            .silent_on_sqlite();
        let q = qs.compile().unwrap();
        let _stmt = Sqlite.compile_select(&q).unwrap();
    });
    assert!(
        !captured.contains("select_for_update modifier dropped"),
        "silent_on_sqlite must suppress the warning, but got:\n{captured}"
    );
}

#[test]
fn no_lock_mode_no_warning() {
    let captured = compile_with_capture(|| {
        let qs = QuerySet::<Job>::default();
        let q = qs.compile().unwrap();
        let _stmt = Sqlite.compile_select(&q).unwrap();
    });
    assert!(
        captured.is_empty() || !captured.contains("select_for_update"),
        "no LockMode means no warning, but got:\n{captured}"
    );
}
