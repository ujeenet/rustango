//! Files only their owner may read, created without following a link.

/// `O_CREAT | O_EXCL`, mode 0600 on Unix: a link or file already at `path`
/// is an error, never opened. Windows has no mode bits here.
///
/// # Errors
/// The open's I/O error; [`std::io::ErrorKind::AlreadyExists`] when
/// anything, a dangling link included, is at `path`.
pub(crate) fn create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
}
