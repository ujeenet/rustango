//! One word-based test for "this key names a secret", shared by the MCP
//! audit and the access-log / span query redaction.

/// Words whose values are hidden. A `_` entry is a run of words; `key`
/// alone hides every `*_key` (`api_key`, `encryption_key`).
pub(crate) const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "pin",
    "token",
    "jwt",
    "bearer",
    "secret",
    "key",
    "apikey",
    "signature",
    "sig",
    "auth",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "session",
    "sessionid",
    "otp",
    "totp",
    "code_verifier",
];

/// The lowercase words of a key, split on `_`, `-` and lower-to-upper
/// case changes: `newPassword` is `new password`, `x-api-key` is `x api key`.
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        let split = matches!(c, '_' | '-') || (prev_lower && c.is_ascii_uppercase());
        if split && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if !matches!(c, '_' | '-') {
            word.push(c.to_ascii_lowercase());
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// `true` when any run of `key`'s words equals the words of an entry.
/// Errs toward hiding: `token_count` matches `token`.
pub(crate) fn matches_any<'a>(key: &str, entries: impl IntoIterator<Item = &'a str>) -> bool {
    let words = key_words(key);
    entries.into_iter().any(|entry| {
        let want = key_words(entry);
        !want.is_empty()
            && words
                .windows(want.len())
                .any(|w| w.iter().zip(&want).all(|(a, b)| a == b))
    })
}

/// `true` when `key` names a secret by [`SECRET_WORDS`].
#[cfg(feature = "mcp")]
pub(crate) fn is_secret(key: &str) -> bool {
    matches_any(key, SECRET_WORDS.iter().copied())
}
