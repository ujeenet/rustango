//! Which column a tenant user signs in with: `[auth] login_field`.

use serde::Deserialize;

/// The identifier tenant users type on the sign-in form, Basic auth and
/// `/auth/login`. `Email` needs the `sso` feature, which adds the column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum LoginField {
    /// `rustango_users.username`, matched exactly.
    #[default]
    Username,
    /// `rustango_users.email`, matched ignoring case.
    #[cfg(feature = "sso")]
    Email,
}

impl LoginField {
    /// The form label: `"Username"` or `"Email"`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Username => "Username",
            #[cfg(feature = "sso")]
            Self::Email => "Email",
        }
    }

    /// The `<input type>` of the sign-in field.
    #[must_use]
    pub fn input_type(self) -> &'static str {
        match self {
            Self::Username => "text",
            #[cfg(feature = "sso")]
            Self::Email => "email",
        }
    }

    /// The `autocomplete` hint of the sign-in field.
    #[must_use]
    pub fn autocomplete(self) -> &'static str {
        match self {
            Self::Username => "username",
            #[cfg(feature = "sso")]
            Self::Email => "email",
        }
    }

    /// The setting's value as written in TOML.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Username => "username",
            #[cfg(feature = "sso")]
            Self::Email => "email",
        }
    }
}

impl std::fmt::Display for LoginField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
