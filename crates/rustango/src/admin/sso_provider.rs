//! Back-compat shim — the `SsoProvider` model + `list_enabled` /
//! `resolve_by_slug` moved to [`crate::sso::provider`] (the admin-independent
//! `sso` feature) so member / end-user SSO can build without the auto-admin.
//!
//! This re-export keeps the historical `crate::admin::sso_provider::*`
//! paths resolving for existing callers (e.g. [`crate::tenancy::sso`],
//! [`crate::admin::urls`]). The table name (`rustango_sso_providers`) and
//! every field attr are unchanged — migrations are unaffected.

pub use crate::sso::provider::*;
