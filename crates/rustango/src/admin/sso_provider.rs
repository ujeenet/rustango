//! Re-export of the `SsoProvider` model and its `list_enabled` and
//! `resolve_by_slug` helpers, which live in [`crate::sso::provider`]
//! so end-user SSO can build without the auto-admin.
//!
//! This keeps the older `crate::admin::sso_provider::…` paths working
//! for callers such as [`crate::tenancy::sso`].

pub use crate::sso::provider::*;
