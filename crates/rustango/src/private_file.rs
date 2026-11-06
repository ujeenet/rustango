//! Files only their owner may read, created without following a link.

/// `O_CREAT | O_EXCL`, mode 0600 on Unix: a link or file already at `path`
/// is an error, never opened. Windows has no mode bits here.
///
/// # Errors
/// The open's I/O error; [`std::io::ErrorKind::AlreadyExists`] when
/// anything, a dangling link included, is at `path`.
pub(crate) fn create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    owner_only().create_new(true).open(path)
}

/// Open `path` for writing, creating it 0600 if absent. A link at `path`
/// is refused (`O_NOFOLLOW` on Unix), never followed.
///
/// # Errors
/// The open's I/O error.
#[cfg(feature = "cache")]
pub(crate) fn open_or_create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = owner_only();
    opts.create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NOFOLLOW);
    opts.open(path)
}

/// Write access, mode 0600 on Unix for a file the open creates.
fn owner_only() -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts
}

#[cfg(all(test, unix, feature = "cache"))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    /// A new file is 0600; an existing one opens without truncating.
    #[test]
    fn open_or_create_is_owner_only_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        drop(super::open_or_create(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::write(&path, "kept").unwrap();
        drop(super::open_or_create(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
    }

    /// A link at the path is refused, and its target never created.
    #[test]
    fn open_or_create_refuses_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let (link, target) = (dir.path().join("l"), dir.path().join("t"));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(super::open_or_create(&link).is_err());
        assert!(!target.exists());
    }
}
