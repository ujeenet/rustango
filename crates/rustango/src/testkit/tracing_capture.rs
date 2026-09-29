//! A `tracing_subscriber` writer that keeps what it was given, so a test
//! can assert on rendered log lines.

use std::sync::{Arc, Mutex, PoisonError};

/// Clones share one buffer. Pass it to `.with_writer(..)` and read it
/// back with [`CaptureWriter::contents`]. Pair it with `with_ansi(false)`,
/// or escape codes land between the characters an assertion looks for.
#[derive(Clone, Default)]
pub struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl CaptureWriter {
    /// Everything written so far, lossily decoded.
    #[must_use]
    pub fn contents(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
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
