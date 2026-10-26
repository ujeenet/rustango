//! One word-based test for "this key names a secret", shared by the MCP
//! audit and the access-log / span query redaction.

/// Words whose values are hidden. A `_` entry is a run of words; `key`
/// alone hides every `*_key` (`api_key`, `encryption_key`).
pub(crate) const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pass",
    "pwd",
    "passphrase",
    "pin",
    "ssn",
    "token",
    "jwt",
    "bearer",
    "secret",
    "key",
    "apikey",
    "privatekey",
    "signature",
    "sig",
    "auth",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "session",
    "sessionid",
    "sessid",
    "otp",
    "totp",
    "code_verifier",
];

/// Names also matched inside a joined key (`accesstoken`, `JSESSIONID`).
/// Short or common words (`key`, `pin`, `auth`) are left out: they sit
/// inside ordinary names like `keyboard` or `author`.
const JOINED_WORDS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "token",
    "secret",
    "apikey",
    "sessionid",
    "sessid",
    "privatekey",
    "credential",
    "signature",
    "authorization",
    "cookie",
];

/// The byte ranges of `key`'s words. A word breaks at any other
/// character, between letters and digits, at a lower-to-upper change
/// and before the last capital of a run (`PASSWORDConfirm`).
fn word_ranges(key: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut prev: Option<char> = None;
    let mut chars = key.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !c.is_alphanumeric() {
            if let Some(s) = start.take() {
                out.push((s, i));
            }
            prev = None;
            continue;
        }
        let next = chars.peek().map(|&(_, n)| n);
        let split = prev.is_some_and(|p| {
            (p.is_lowercase() && c.is_uppercase())
                || (p.is_alphabetic() != c.is_alphabetic())
                || (p.is_uppercase()
                    && c.is_uppercase()
                    && next.is_some_and(char::is_lowercase))
        });
        if split {
            if let Some(s) = start.take() {
                out.push((s, i));
            }
        }
        start.get_or_insert(i);
        prev = Some(c);
    }
    if let Some(s) = start {
        out.push((s, key.len()));
    }
    out
}

/// A list of secret names, split once so a lookup only splits the key.
#[derive(Clone, Debug, Default)]
pub(crate) struct SecretNames {
    /// Each name's lowercase words.
    runs: Vec<Box<[Box<str>]>>,
    /// Lowercase alphanumeric forms also matched inside a joined key.
    joined: Vec<Box<str>>,
}

impl SecretNames {
    pub(crate) fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::default();
        for name in names {
            let words: Box<[Box<str>]> = word_ranges(name)
                .into_iter()
                .map(|(a, b)| name[a..b].to_lowercase().into_boxed_str())
                .collect();
            if words.is_empty() || out.runs.contains(&words) {
                continue;
            }
            let flat: String = words.concat();
            // A multi-word name is specific enough to find inside a key.
            if (words.len() > 1 || JOINED_WORDS.contains(&flat.as_str()))
                && !out.joined.iter().any(|j| **j == *flat)
            {
                out.joined.push(flat.into_boxed_str());
            }
            out.runs.push(words);
        }
        out
    }

    /// `true` when a run of `key`'s words is a name, or the key joined
    /// up contains a long name. Errs toward hiding: `token_count` matches.
    pub(crate) fn matches(&self, key: &str) -> bool {
        let lower = key.to_lowercase();
        // Lowering keeps ASCII byte offsets, so split the original and
        // keep its case breaks; other text is split lowered.
        let ranges = if key.is_ascii() {
            word_ranges(key)
        } else {
            word_ranges(&lower)
        };
        let words: Vec<&str> = ranges.iter().map(|&(a, b)| &lower[a..b]).collect();
        let by_word = self.runs.iter().any(|want| {
            words
                .windows(want.len())
                .any(|w| w.iter().zip(want.iter()).all(|(a, b)| *a == &**b))
        });
        by_word || {
            let flat: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
            self.joined.iter().any(|j| flat.contains(&**j))
        }
    }
}

/// `true` when `key` names a secret by [`SECRET_WORDS`].
#[cfg(feature = "mcp")]
pub(crate) fn is_secret(key: &str) -> bool {
    static NAMES: std::sync::LazyLock<SecretNames> =
        std::sync::LazyLock::new(|| SecretNames::new(SECRET_WORDS.iter().copied()));
    NAMES.matches(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> SecretNames {
        SecretNames::new(SECRET_WORDS.iter().copied())
    }

    #[test]
    fn hides_secret_names_in_any_spelling() {
        let names = defaults();
        for k in [
            "password1",
            "password2",
            "token2",
            "user[password]",
            "data[token]",
            "user.password",
            "accesstoken",
            "refreshtoken",
            "authtoken",
            "clientsecret",
            "secretkey",
            "privatekey",
            "xapikey",
            "csrftoken",
            "JSESSIONID",
            "PHPSESSID",
            "PASSWORDConfirm",
            "ssn",
            "pass",
            "X-Api-Key",
            "newPassword",
            "api_key",
        ] {
            assert!(names.matches(k), "{k} must be hidden");
        }
    }

    #[test]
    fn leaves_ordinary_names_alone() {
        let names = defaults();
        for k in ["keyboard", "page", "sort_by", "monkey", "author", "pinned", "passport"] {
            assert!(!names.matches(k), "{k} must stay visible");
        }
    }

    #[test]
    fn splits_on_every_boundary() {
        let words = |k: &str| -> Vec<String> {
            word_ranges(k).iter().map(|&(a, b)| k[a..b].to_owned()).collect()
        };
        assert_eq!(words("PASSWORDConfirm"), ["PASSWORD", "Confirm"]);
        assert_eq!(words("token2"), ["token", "2"]);
        assert_eq!(words("user[password]"), ["user", "password"]);
        assert_eq!(words("newPassword"), ["new", "Password"]);
    }
}
