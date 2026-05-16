//! Pluggable password validator chain — Django's
//! `AUTH_PASSWORD_VALIDATORS = [...]` setting.
//!
//! Plug an ordered list of [`PasswordValidator`]s into a
//! [`PasswordValidatorChain`]; call [`PasswordValidatorChain::validate`]
//! at signup / password-change time. The chain runs *every*
//! validator and accumulates errors so the user sees the full
//! problem list in one round-trip, not one failure at a time.
//!
//! ```ignore
//! use rustango::password_validators::{
//!     PasswordValidatorChain, MinimumLengthValidator,
//!     NumericPasswordValidator, UserAttributes,
//! };
//!
//! let chain = PasswordValidatorChain::new()
//!     .with(Box::new(MinimumLengthValidator::new(8)))
//!     .with(Box::new(NumericPasswordValidator));
//!
//! let attrs = UserAttributes::new().with("username", "alice");
//! chain.validate("hunter2", &attrs)?;
//! ```
//!
//! ## Built-in validators
//! - [`MinimumLengthValidator`] — Django's `MinimumLengthValidator`.
//! - [`MaximumLengthValidator`] — extra rustango safeguard; some
//!   password hashers cap input length, so the validator surfaces
//!   "too long" before the hasher does.
//! - [`NumericPasswordValidator`] — Django's same-named validator.
//! - [`UserAttributeSimilarityValidator`] — Django's same. Rejects
//!   passwords that contain (or are too similar to) a value from
//!   the user attributes bag.
//! - [`CommonPasswordValidator`] — small built-in top-N list. The
//!   real Django uses ~20k entries; this ship the top-100 to keep
//!   the binary lean. Pass `with_list(...)` for a longer list.
//!
//! Custom validators implement [`PasswordValidator`] (one method).
//!
//! Issue #54 — second piece of [`crate::auth_backends`].

use std::collections::HashMap;
use std::fmt;

// ------------------------------------------------------------------ UserAttributes

/// Per-user bag of attribute values that
/// [`UserAttributeSimilarityValidator`] checks the password against.
/// Typically `username` + `email` + display name — anything an
/// attacker is likely to guess from the user's public profile.
#[derive(Debug, Default, Clone)]
pub struct UserAttributes {
    pub values: HashMap<String, String>,
}

impl UserAttributes {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Iterate (key, value) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

// ------------------------------------------------------------------ ValidationError

/// One validator's complaint. Each entry has a stable `code` (for
/// programmatic checks / template branches) and a human `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub code: &'static str,
    pub message: String,
}

impl ValidationError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Accumulated errors from the chain. Use [`Self::is_empty`] to gate
/// the success path; iterate `errors` to render every complaint.
#[derive(Debug, Default, Clone)]
pub struct ValidationErrors {
    pub errors: Vec<ValidationError>,
}

impl ValidationErrors {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, err: ValidationError) {
        self.errors.push(err);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// `true` when any error carries this code.
    #[must_use]
    pub fn has_code(&self, code: &str) -> bool {
        self.errors.iter().any(|e| e.code == code)
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.errors.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

// ------------------------------------------------------------------ PasswordValidator

/// One link in the chain. `validate` returns `Ok(())` on pass; one
/// [`ValidationError`] on failure. The chain runs every validator
/// and collects errors (not short-circuit) so users see the full
/// problem set per round-trip.
pub trait PasswordValidator: Send + Sync {
    /// Stable identifier (`"length"`, `"numeric"`, …). Useful for
    /// docs, tests, and template branches.
    fn code(&self) -> &'static str;

    /// Validate `password` against `user_attrs`. Implementations
    /// SHOULD NOT log or store the password.
    fn validate(&self, password: &str, user_attrs: &UserAttributes) -> Result<(), ValidationError>;

    /// Help text the UI shows next to the password field. Default
    /// empty — override to surface the validator's requirement.
    fn help_text(&self) -> String {
        String::new()
    }
}

// ------------------------------------------------------------------ PasswordValidatorChain

/// Ordered list of validators run together.
#[derive(Default)]
pub struct PasswordValidatorChain {
    validators: Vec<Box<dyn PasswordValidator>>,
}

impl PasswordValidatorChain {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a validator.
    #[must_use]
    pub fn with(mut self, v: Box<dyn PasswordValidator>) -> Self {
        self.validators.push(v);
        self
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.validators.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Run every validator. Returns `Ok(())` only when ALL pass.
    /// On any failure, returns the full accumulated error list.
    pub fn validate(
        &self,
        password: &str,
        user_attrs: &UserAttributes,
    ) -> Result<(), ValidationErrors> {
        let mut errs = ValidationErrors::new();
        for v in &self.validators {
            if let Err(e) = v.validate(password, user_attrs) {
                errs.push(e);
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// Concatenated help text from every validator (for rendering
    /// in the signup form).
    #[must_use]
    pub fn help_text(&self) -> Vec<String> {
        self.validators
            .iter()
            .map(|v| v.help_text())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

// ------------------------------------------------------------------ MinimumLengthValidator

/// Reject passwords shorter than `min_length` characters
/// (Unicode scalar values, not bytes).
pub struct MinimumLengthValidator {
    pub min_length: usize,
}

impl MinimumLengthValidator {
    #[must_use]
    pub fn new(min_length: usize) -> Self {
        Self { min_length }
    }
}

impl PasswordValidator for MinimumLengthValidator {
    fn code(&self) -> &'static str {
        "password_too_short"
    }
    fn validate(&self, password: &str, _: &UserAttributes) -> Result<(), ValidationError> {
        if password.chars().count() < self.min_length {
            Err(ValidationError::new(
                "password_too_short",
                format!(
                    "Password must be at least {} characters long.",
                    self.min_length
                ),
            ))
        } else {
            Ok(())
        }
    }
    fn help_text(&self) -> String {
        format!(
            "Your password must contain at least {} characters.",
            self.min_length
        )
    }
}

// ------------------------------------------------------------------ MaximumLengthValidator

/// Reject passwords longer than `max_length` characters. Pairs with
/// hasher input limits (Argon2 has no hard cap but stash-anywhere
/// patterns benefit from one).
pub struct MaximumLengthValidator {
    pub max_length: usize,
}

impl MaximumLengthValidator {
    #[must_use]
    pub fn new(max_length: usize) -> Self {
        Self { max_length }
    }
}

impl PasswordValidator for MaximumLengthValidator {
    fn code(&self) -> &'static str {
        "password_too_long"
    }
    fn validate(&self, password: &str, _: &UserAttributes) -> Result<(), ValidationError> {
        if password.chars().count() > self.max_length {
            Err(ValidationError::new(
                "password_too_long",
                format!(
                    "Password must be at most {} characters long.",
                    self.max_length
                ),
            ))
        } else {
            Ok(())
        }
    }
    fn help_text(&self) -> String {
        format!(
            "Your password may contain at most {} characters.",
            self.max_length
        )
    }
}

// ------------------------------------------------------------------ NumericPasswordValidator

/// Reject passwords made entirely of digits (`"12345678"`). Django
/// rationale: pure-digit passwords are trivially brute-forceable
/// even at long lengths, and users default to them when forced into
/// "must be 8 chars" without a complexity rule.
pub struct NumericPasswordValidator;

impl PasswordValidator for NumericPasswordValidator {
    fn code(&self) -> &'static str {
        "password_entirely_numeric"
    }
    fn validate(&self, password: &str, _: &UserAttributes) -> Result<(), ValidationError> {
        if !password.is_empty() && password.chars().all(|c| c.is_ascii_digit()) {
            Err(ValidationError::new(
                "password_entirely_numeric",
                "Password may not be entirely numeric.",
            ))
        } else {
            Ok(())
        }
    }
    fn help_text(&self) -> String {
        "Your password can't be entirely numeric.".to_owned()
    }
}

// ------------------------------------------------------------------ UserAttributeSimilarityValidator

/// Reject passwords that contain (case-insensitive) one of the user's
/// attributes — username, email local-part, display name — as a
/// substring of length ≥ `threshold` chars. Defaults: `threshold=4`,
/// checks every attribute.
pub struct UserAttributeSimilarityValidator {
    /// Minimum overlap length to reject. Below this is too short to
    /// signal "the user reused their username".
    pub threshold: usize,
    /// Only check these attribute keys. Empty = check every attribute.
    pub user_attributes: Vec<String>,
}

impl Default for UserAttributeSimilarityValidator {
    fn default() -> Self {
        Self {
            threshold: 4,
            user_attributes: Vec::new(),
        }
    }
}

impl UserAttributeSimilarityValidator {
    #[must_use]
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn only(mut self, attrs: Vec<String>) -> Self {
        self.user_attributes = attrs;
        self
    }

    fn attrs<'a>(&'a self, all: &'a UserAttributes) -> impl Iterator<Item = (&'a str, &'a str)> {
        all.iter().filter(move |(k, _)| {
            self.user_attributes.is_empty() || self.user_attributes.iter().any(|a| a == *k)
        })
    }
}

impl PasswordValidator for UserAttributeSimilarityValidator {
    fn code(&self) -> &'static str {
        "password_too_similar"
    }

    fn validate(&self, password: &str, user_attrs: &UserAttributes) -> Result<(), ValidationError> {
        let lower_password = password.to_lowercase();
        for (_, value) in self.attrs(user_attrs) {
            // Check both whole attribute + each splittable chunk
            // (so `john.doe@example.com` rejects both `john.doe`,
            // `john`, `doe`, and `example`).
            let lower = value.to_lowercase();
            for piece in std::iter::once(lower.as_str())
                .chain(lower.split(|c: char| !c.is_alphanumeric()))
                .filter(|p| p.chars().count() >= self.threshold)
            {
                if lower_password.contains(piece) {
                    return Err(ValidationError::new(
                        "password_too_similar",
                        "Password is too similar to a user attribute.",
                    ));
                }
            }
        }
        Ok(())
    }

    fn help_text(&self) -> String {
        "Your password can't be too similar to your other personal information.".to_owned()
    }
}

// ------------------------------------------------------------------ CommonPasswordValidator

/// Reject passwords from a stash list. Ships with the [top 100
/// most-common passwords](https://en.wikipedia.org/wiki/List_of_the_most_common_passwords);
/// real deployments pass a longer list via [`Self::with_list`].
pub struct CommonPasswordValidator {
    list: Vec<String>,
}

impl Default for CommonPasswordValidator {
    fn default() -> Self {
        // Curated subset of the top-100 globally (case-insensitive).
        // Real Django uses a 20k-entry list; ship the most-likely
        // here and let callers swap in a longer one.
        let list = [
            "123456",
            "123456789",
            "12345678",
            "12345",
            "qwerty",
            "qwerty123",
            "1q2w3e",
            "password",
            "password1",
            "password123",
            "admin",
            "admin123",
            "letmein",
            "welcome",
            "monkey",
            "dragon",
            "iloveyou",
            "abc123",
            "111111",
            "1234567",
            "1234567890",
            "000000",
            "sunshine",
            "princess",
            "football",
            "baseball",
            "shadow",
            "master",
            "jordan",
            "michael",
            "superman",
            "batman",
            "trustno1",
            "freedom",
            "passw0rd",
            "qwertyuiop",
            "asdfghjkl",
            "zxcvbnm",
            "1qaz2wsx",
            "qazwsx",
            "killer",
            "hello",
            "login",
            "starwars",
            "whatever",
            "hottie",
            "loveme",
            "zaq12wsx",
            "f4cebook",
            "google",
            "lovely",
            "ashley",
            "nicole",
            "andrew",
            "qwerty1",
            "donald",
            "qwertyu",
            "asdf",
            "asdfgh",
            "asdfghjk",
            "biteme",
            "computer",
            "internet",
            "samsung",
            "hunter",
            "hunter2",
            "secret",
            "tigger",
            "thomas",
            "robert",
            "soccer",
            "lakers",
            "pokemon",
            "matrix",
            "blink182",
            "harley",
            "ranger",
            "buster",
            "summer",
            "george",
            "fuckyou",
            "fuckme",
            "654321",
            "555555",
            "888888",
            "987654321",
            "121212",
            "112233",
        ];
        Self {
            list: list.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

impl CommonPasswordValidator {
    /// Use the bundled top-N list. Same as `Default::default()`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a caller-provided list. Entries are matched case-
    /// insensitively after trimming whitespace.
    #[must_use]
    pub fn with_list<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            list: entries
                .into_iter()
                .map(|s| s.into().trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}

impl PasswordValidator for CommonPasswordValidator {
    fn code(&self) -> &'static str {
        "password_too_common"
    }
    fn validate(&self, password: &str, _: &UserAttributes) -> Result<(), ValidationError> {
        let lower = password.trim().to_lowercase();
        if self.list.iter().any(|p| p == &lower) {
            return Err(ValidationError::new(
                "password_too_common",
                "Password is too common.",
            ));
        }
        Ok(())
    }
    fn help_text(&self) -> String {
        "Your password can't be a commonly used password.".to_owned()
    }
}

// ------------------------------------------------------------------ Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_length_rejects_short() {
        let v = MinimumLengthValidator::new(8);
        let err = v.validate("short", &UserAttributes::new()).unwrap_err();
        assert_eq!(err.code, "password_too_short");
        assert!(v
            .validate("longenoughpassword", &UserAttributes::new())
            .is_ok());
    }

    #[test]
    fn minimum_length_counts_unicode_scalars_not_bytes() {
        // 4 emoji = 4 chars (each 4 bytes); validator with min=5 rejects.
        let v = MinimumLengthValidator::new(5);
        assert!(v.validate("🎉🎉🎉🎉", &UserAttributes::new()).is_err());
        assert!(v.validate("🎉🎉🎉🎉🎉", &UserAttributes::new()).is_ok());
    }

    #[test]
    fn maximum_length_rejects_long() {
        let v = MaximumLengthValidator::new(10);
        let err = v
            .validate("this is much too long", &UserAttributes::new())
            .unwrap_err();
        assert_eq!(err.code, "password_too_long");
        assert!(v.validate("ok", &UserAttributes::new()).is_ok());
    }

    #[test]
    fn numeric_rejects_only_pure_digit() {
        let v = NumericPasswordValidator;
        assert!(v.validate("12345678", &UserAttributes::new()).is_err());
        assert!(v.validate("12345abc", &UserAttributes::new()).is_ok());
        // Empty string is the "no password supplied" case — let the
        // min-length validator complain about that.
        assert!(v.validate("", &UserAttributes::new()).is_ok());
    }

    #[test]
    fn user_attribute_similarity_rejects_username() {
        let v = UserAttributeSimilarityValidator::new(4);
        let attrs = UserAttributes::new().with("username", "alice");
        // password contains "alice" — reject.
        let err = v.validate("alice12345", &attrs).unwrap_err();
        assert_eq!(err.code, "password_too_similar");
        // password unrelated — pass.
        assert!(v.validate("hunter77!", &attrs).is_ok());
    }

    #[test]
    fn user_attribute_similarity_case_insensitive() {
        let v = UserAttributeSimilarityValidator::new(4);
        let attrs = UserAttributes::new().with("username", "Alice");
        assert!(v.validate("ALICE-secret", &attrs).is_err());
        assert!(v.validate("alice-secret", &attrs).is_err());
    }

    #[test]
    fn user_attribute_similarity_splits_email_pieces() {
        let v = UserAttributeSimilarityValidator::new(4);
        let attrs = UserAttributes::new().with("email", "john.doe@example.com");
        // chunks: "john", "doe", "example", "com" (com filtered by threshold)
        assert!(
            v.validate("john2025", &attrs).is_err(),
            "should reject password containing email local-part chunk"
        );
        assert!(
            v.validate("example-thing", &attrs).is_err(),
            "should reject password containing email domain chunk"
        );
        assert!(v.validate("zphyrr12", &attrs).is_ok());
    }

    #[test]
    fn user_attribute_similarity_respects_only_list() {
        let v = UserAttributeSimilarityValidator::new(4).only(vec!["username".into()]);
        let attrs = UserAttributes::new()
            .with("username", "alice")
            .with("display_name", "Alice Smith");
        // username check rejects.
        assert!(v.validate("alice-pw", &attrs).is_err());
        // display_name is NOT in the only-list, so a password with
        // "smith" doesn't trip the validator.
        assert!(v.validate("smith-pw", &attrs).is_ok());
    }

    #[test]
    fn user_attribute_similarity_threshold_skips_short_chunks() {
        let v = UserAttributeSimilarityValidator::new(5);
        let attrs = UserAttributes::new().with("username", "joe");
        // "joe" is 3 chars, threshold is 5 → not checked.
        assert!(v.validate("joe-secret", &attrs).is_ok());
    }

    #[test]
    fn common_password_rejects_bundled_list() {
        let v = CommonPasswordValidator::new();
        for bad in ["password", "PASSWORD", "qwerty", "iloveyou", "hunter2"] {
            assert!(
                v.validate(bad, &UserAttributes::new()).is_err(),
                "bundled list should reject {bad}"
            );
        }
        assert!(v
            .validate("z9F!quirkysunset", &UserAttributes::new())
            .is_ok());
    }

    #[test]
    fn common_password_custom_list() {
        let v = CommonPasswordValidator::with_list(["my-secret", "company2024"]);
        assert!(v.validate("my-secret", &UserAttributes::new()).is_err());
        assert!(v.validate("MY-SECRET", &UserAttributes::new()).is_err());
        // not on the custom list — pass.
        assert!(v.validate("hunter2", &UserAttributes::new()).is_ok());
    }

    #[test]
    fn chain_accumulates_every_error() {
        let chain = PasswordValidatorChain::new()
            .with(Box::new(MinimumLengthValidator::new(12)))
            .with(Box::new(NumericPasswordValidator))
            .with(Box::new(CommonPasswordValidator::new()));
        // "123456" hits all three: too short + entirely numeric + common.
        let err = chain
            .validate("123456", &UserAttributes::new())
            .unwrap_err();
        assert_eq!(err.len(), 3);
        assert!(err.has_code("password_too_short"));
        assert!(err.has_code("password_entirely_numeric"));
        assert!(err.has_code("password_too_common"));
    }

    #[test]
    fn chain_passes_when_every_validator_passes() {
        let chain = PasswordValidatorChain::new()
            .with(Box::new(MinimumLengthValidator::new(10)))
            .with(Box::new(NumericPasswordValidator))
            .with(Box::new(CommonPasswordValidator::new()));
        assert!(chain
            .validate("z9F!quirkysunset", &UserAttributes::new())
            .is_ok());
    }

    #[test]
    fn empty_chain_always_passes() {
        let chain = PasswordValidatorChain::new();
        assert!(chain.is_empty());
        // Even a `""` empty password passes when there are no
        // validators registered — the chain is purely additive.
        assert!(chain.validate("", &UserAttributes::new()).is_ok());
    }

    #[test]
    fn help_text_collects_every_non_empty_message() {
        let chain = PasswordValidatorChain::new()
            .with(Box::new(MinimumLengthValidator::new(8)))
            .with(Box::new(NumericPasswordValidator));
        let texts = chain.help_text();
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().any(|t| t.contains("8 characters")));
        assert!(texts.iter().any(|t| t.contains("entirely numeric")));
    }

    #[test]
    fn validation_errors_display_is_separator_joined() {
        let mut errs = ValidationErrors::new();
        errs.push(ValidationError::new("a", "alpha"));
        errs.push(ValidationError::new("b", "beta"));
        let s = format!("{errs}");
        assert_eq!(s, "alpha (a); beta (b)");
    }
}
