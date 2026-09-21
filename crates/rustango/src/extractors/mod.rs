//! Tenancy-aware extractors for handlers.
//!
//! | Extractor | What it gives you |
//! |---|---|
//! | [`Tenant`] | Tenant-scoped DB connection (org + pool) |
//! | [`SessionUser`] | Browser-session tenant user (`None` = anonymous) |
//! | [`SessionOperator`] | Browser-session operator (`None` = anonymous) |
//!
//! They all read request extensions that
//! [`crate::server::Builder`] fills in, so there is no state to wire
//! up.
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
//!
//! [`Tenant`]: crate::extractors::Tenant
//! [`SessionUser`]: crate::extractors::SessionUser
//! [`SessionOperator`]: crate::extractors::SessionOperator

mod database_tenant;
// `SessionUser` and `SessionOperator` work on every backend: they go
// through `FetcherPool` against the `Pool` enum. Schema-mode tenancy
// is still PG-only, but database-mode tenants on SQLite and MySQL
// get the same extractors.
mod session_user;
mod tenant;

pub use database_tenant::{DatabaseTenant, DatabaseTenantContext, DatabaseTenantRejection};
pub use session_user::{SessionOperator, SessionUser};
pub use tenant::{Tenant, TenantContext, TenantRejection};
