//! Pluggable password validator chain.
//!
//! Put an ordered list of [`PasswordValidator`]s into a
//! [`PasswordValidatorChain`] and call
//! [`PasswordValidatorChain::validate`] at signup and password change.
//! The chain runs every validator and collects the errors, so the user
//! sees the whole problem list at once.
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
//! - [`MinimumLengthValidator`]
//! - [`MaximumLengthValidator`] — extra in rustango. Some hashers cap
//!   input length, so this reports "too long" before the hasher does.
//! - [`NumericPasswordValidator`]
//! - [`UserAttributeSimilarityValidator`] — rejects a password that
//!   contains, or is close to, one of the user's attributes.
//! - [`CommonPasswordValidator`] — bundles a short top-100 list
//!   Pass a longer one to `with_list(...)`.
//!
//! Write your own by implementing [`PasswordValidator`] (one method).
//!
//! [`PasswordValidator`]: crate::password_validators::PasswordValidator
//! [`PasswordValidatorChain`]: crate::password_validators::PasswordValidatorChain
//! [`PasswordValidatorChain::validate`]: crate::password_validators::PasswordValidatorChain::validate
//! [`MinimumLengthValidator`]: crate::password_validators::MinimumLengthValidator
//! [`MaximumLengthValidator`]: crate::password_validators::MaximumLengthValidator
//! [`NumericPasswordValidator`]: crate::password_validators::NumericPasswordValidator
//! [`UserAttributeSimilarityValidator`]: crate::password_validators::UserAttributeSimilarityValidator
//! [`CommonPasswordValidator`]: crate::password_validators::CommonPasswordValidator

use std::collections::HashMap;
use std::fmt;

// ------------------------------------------------------------------ UserAttributes

/// The user's attribute values that
/// [`UserAttributeSimilarityValidator`] checks a password against.
/// Usually username, email and display name: anything an attacker can
/// read off a public profile and try.
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

/// One validator's complaint: a stable `code` to branch on and a
/// `message` to show.
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

/// Errors collected by the chain. Gate the success path on
/// [`Self::is_empty`]; iterate `errors` to show them all.
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

/// One link in the chain. `validate` returns `Ok(())` on pass, or one
/// [`ValidationError`] on failure. The chain does not stop at the
/// first failure.
pub trait PasswordValidator: Send + Sync {
    /// Stable identifier (`"length"`, `"numeric"`, …) to branch on.
    fn code(&self) -> &'static str;

    /// Validate `password` against `user_attrs`. Implementations
    /// SHOULD NOT log or store the password.
    fn validate(&self, password: &str, user_attrs: &UserAttributes) -> Result<(), ValidationError>;

    /// Help text shown next to the password field. Empty by default;
    /// override it to state the rule.
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

    /// Run every validator. `Ok(())` only when all pass; otherwise
    /// the full list of errors.
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

    /// Help text from every validator, for the signup form.
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

/// Reject passwords longer than `max_length` characters. Argon2 has
/// no hard cap, but a limit keeps a huge input from reaching the
/// hasher at all.
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

/// Reject passwords made only of digits (`"12345678"`). Users fall
/// back to these under a bare length rule, and a digits-only space is
/// small enough to brute-force even when it is long.
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

/// Reject a password that contains one of the user's attributes —
/// username, email local-part, display name — as a piece of
/// `threshold` characters or more. Case-insensitive. By default
/// `threshold` is 4 and every attribute is checked.
pub struct UserAttributeSimilarityValidator {
    /// Shortest overlap that counts. Anything shorter is too weak a
    /// signal that the user reused their own name.
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
            // Check the whole attribute and each chunk, so
            // `john.doe@example.com` also rejects `john`, `doe` and
            // `example`.
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

/// Reject passwords on a known-bad list. Bundles the [top 100
/// most-common passwords](https://en.wikipedia.org/wiki/List_of_the_most_common_passwords);
/// pass a longer list in production with [`Self::with_list`].
pub struct CommonPasswordValidator {
    list: Vec<String>,
}

impl Default for CommonPasswordValidator {
    fn default() -> Self {
        // Subset of the global top-100, matched case-insensitively.
        // Callers can swap in a longer list.
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
    /// Use the bundled list. Same as `Default::default()`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Use your own list. Entries are trimmed and matched without
    /// regard to case.
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
        // An empty string means "no password supplied"; that is the
        // min-length validator's complaint, not this one's.
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
        // display_name is not on the only-list, so "smith" passes.
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
        // With no validators registered even `""` passes: the chain
        // only adds rules, it has none of its own.
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
