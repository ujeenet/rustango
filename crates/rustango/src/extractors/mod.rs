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
#[cfg(feature = "postgres")]
mod session_user;
#[cfg(feature = "postgres")]
mod tenant;

pub use database_tenant::{DatabaseTenant, DatabaseTenantContext, DatabaseTenantRejection};
#[cfg(feature = "postgres")]
pub use session_user::{SessionOperator, SessionUser};
#[cfg(feature = "postgres")]
pub use tenant::{Tenant, TenantContext, TenantRejection};
