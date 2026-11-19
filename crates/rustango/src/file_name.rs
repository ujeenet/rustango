//! Client file names made safe as storage keys, shared by `uploads` and `media`.

/// Longest name [`sanitize_filename`] returns, in bytes: fits a
/// `VARCHAR(255)` key with a prefix and a UUID, and any filesystem name limit.
pub const MAX_FILENAME_BYTES: usize = 100;

/// Longest extension (with its dot) a capped name keeps.
const MAX_KEPT_EXT_BYTES: usize = 16;

/// Drop directory parts and replace anything outside `[a-zA-Z0-9._-]`
/// with `_`, so the result is safe to use as a storage key. This is
/// what stops a client-supplied path from escaping the upload prefix.
/// A name longer than [`MAX_FILENAME_BYTES`] is cut, keeping its extension.
pub fn sanitize_filename(name: &str) -> String {
    // Keep the basename only; clients sometimes send a full path.
    // Split on both separators instead of `Path::file_name`, which
    // treats `\` as a separator only on Windows. Otherwise the same
    // upload gets a different name depending on the server's OS.
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    let mut out = String::with_capacity(base.len().min(MAX_FILENAME_BYTES));
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("upload");
    }
    cap(out)
}

/// Cut `name` to [`MAX_FILENAME_BYTES`] on a char boundary, keeping a short extension.
fn cap(name: String) -> String {
    if name.len() <= MAX_FILENAME_BYTES {
        return name;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if name.len() - i <= MAX_KEPT_EXT_BYTES => name.split_at(i),
        _ => (name.as_str(), ""),
    };
    let mut keep = MAX_FILENAME_BYTES - ext.len();
    while !stem.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{ext}", &stem[..keep])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_directory_and_unsafe_chars() {
        assert_eq!(sanitize_filename("../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("My File.png"), "My_File.png");
        assert_eq!(sanitize_filename("évil.jpg"), "_vil.jpg");
        assert_eq!(sanitize_filename(""), "upload");
        // Windows separators, asserted on every platform.
        assert_eq!(sanitize_filename("C:\\windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("C:/Users/me/photo.jpg"), "photo.jpg");
    }

    #[test]
    fn a_long_name_is_capped_and_keeps_its_extension() {
        let long = format!("{}.jpeg", "a".repeat(500));
        let out = sanitize_filename(&long);
        assert_eq!(out.len(), MAX_FILENAME_BYTES);
        assert!(out.ends_with("a.jpeg"), "{out}");
        let wide = format!("{}.png", "é".repeat(300));
        let out = sanitize_filename(&wide);
        assert_eq!(out.len(), MAX_FILENAME_BYTES);
        assert!(out.ends_with("_.png"), "{out}");
    }

    #[test]
    fn a_long_extension_is_cut_with_the_name() {
        let out = sanitize_filename(&format!("a.{}", "b".repeat(300)));
        assert_eq!(out.len(), MAX_FILENAME_BYTES);
        assert!(out.starts_with("a.bbb"), "{out}");
    }

    #[test]
    fn a_short_name_is_unchanged() {
        let name = format!("{}.txt", "a".repeat(MAX_FILENAME_BYTES - 4));
        assert_eq!(sanitize_filename(&name), name);
    }

    #[test]
    fn cap_stays_on_a_char_boundary() {
        let out = cap("é".repeat(80));
        assert!(out.len() <= MAX_FILENAME_BYTES && out.chars().all(|c| c == 'é'));
    }
}
