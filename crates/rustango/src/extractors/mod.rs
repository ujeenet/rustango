//! Per-request extractors for handlers — tenancy-aware DI.
//!
//! | Extractor | What it gives you |
//! |---|---|
//! | [`Tenant`] | Tenant-scoped DB connection (org + pool) |
//! | [`SessionUser`] | Browser-session tenant user (`None` = anonymous) |
//! | [`SessionOperator`] | Browser-session operator (`None` = anonymous) |
//!
//! All extractors read from request extensions populated by
//! [`crate::server::Builder`], so no state wiring is required.
//!
//! ```ignore
//! use rustango::extractors::{Tenant, SessionUser};
//!
//! pub async fn my_handler(
//!     mut t: Tenant,
//!     SessionUser(user): SessionUser,
//! ) -> impl IntoResponse {
//!     match user {
//!         Some(u) => format!("hello, {}", u.username).into_response(),
//!         None    => StatusCode::UNAUTHORIZED.into_response(),
//!     }
//! }
//! ```

mod database_tenant;
// v0.41 (#317) — `session_user` is now tri-dialect. Both
// `SessionUser` and `SessionOperator` route through `FetcherPool`
// against the tri-dialect `Pool` enum, so the module no longer
// needs the `feature = "postgres"` gate. Schema-mode tenancy
// remains PG-only by language (TenantPools rejects schema-mode for
// non-PG backends at runtime), but database-mode tenants on
// sqlite/mysql get the same browser-session extractors as PG.
mod session_user;
mod tenant;

pub use database_tenant::{DatabaseTenant, DatabaseTenantContext, DatabaseTenantRejection};
pub use session_user::{SessionOperator, SessionUser};
pub use tenant::{Tenant, TenantContext, TenantRejection};
