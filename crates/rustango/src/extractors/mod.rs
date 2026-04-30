//! Per-request extractors for handlers — tenancy-aware DI.
//!
//! Today this module ships the [`Tenant`] extractor only. Future
//! roadmap slices will add `Operator` and `User` (per-tenant session
//! users). All extractors read from request extensions populated by
//! [`crate::server::Builder`], so the user does not have to thread
//! state through `with_state`.
//!
//! ```ignore
//! use rustango::extractors::Tenant;
//!
//! pub async fn list_articles(mut t: Tenant) -> impl IntoResponse {
//!     let posts: Vec<Post> = Post::objects().fetch_on(t.conn()).await?;
//!     // ...
//! }
//! ```

mod tenant;

pub use tenant::{Tenant, TenantContext, TenantRejection};
