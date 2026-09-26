//! Axum REST router for the [`MediaManager`] surface — the **internal
//! management API**.
//!
//! Requires the **`admin`** feature as well as `media`.
//!
//! # What this router is for
//!
//! Uploading, deleting, moving, tagging and browsing the library, from
//! inside your app or your admin. Every route is an operator action,
//! and every response with a media row carries a **presigned** URL.
//! That is why the whole surface sends `Cache-Control: no-store`.
//!
//! # What it is not for
//!
//! **Serving media to the public.** No route here is anonymous;
//! [`MediaPerms`] answers `401` when there is no principal. A public
//! page showing an uploaded image does not call this API. It renders a
//! URL its own handler computed:
//!
//! ```ignore
//! // your own public route, outside this router
//! let url = manager.public_url(id).await?;   // CDN address, no signature
//! ```
//!
//! See [`crate::media::MediaManager::public_url`] and `docs/files.md`.
//! Do not reach for an `AllowAll` [`MediaAuthorizer`] to make a public
//! page work: it opens all sixteen routes, including `DELETE` and the
//! presigned `PUT`, to everyone.
//!
//! # This router requires an authorization policy
//!
//! Build it with [`media_router_with`] and supply a
//! [`MediaAuthorizer`]. [`media_router`] is deprecated and refuses
//! every request — it does not serve.
//!
//! Without a policy these routes take no authentication and no tenant.
//! An anonymous caller could walk the id space collecting presigned S3
//! download links, delete rows by id, and mint a presigned PUT for any
//! key prefix.
//!
//! A blanket `.layer(auth)` in front is **not** enough for a
//! multi-tenant deployment: no handler carries a tenant, so an
//! authenticated tenant-A user still reads tenant B's row by id.
//! [`MediaTarget`] names the row so the decision can be made per row.
//!
//! Mount it under any prefix; `/media` is conventional. All responses
//! are JSON (no auto-CSRF — wire that on the outer router via
//! [`crate::forms::csrf`]).
//!
//! ## Endpoints
//!
//! | Method | Path                              | Purpose |
//! |--------|-----------------------------------|---------|
//! | POST   | `/uploads/begin`                  | Start a direct browser upload — returns `{media_id, upload_url, expires_at}`. |
//! | POST   | `/uploads/{id}/finalize`          | Confirm the storage object landed; flips the row Pending→Ready. |
//! | GET    | `/media/{id}`                     | Single Media row + URL + presigned link. |
//! | DELETE | `/media/{id}`                     | Soft-delete the Media row (storage preserved). |
//! | POST   | `/media/{id}/move`                | Move Media to another collection: body `{collection_id?: i64}`. |
//! | POST   | `/media/{id}/tags`                | Replace tag set: body `{slugs: ["a","b"]}`. |
//! | DELETE | `/media/{id}/tags/{slug}`         | Remove a single tag. |
//! | POST   | `/collections`                    | Create: body `{name, slug, parent_id?, description?}`. Authorized as `Add(NewCollection)`. |
//! | GET    | `/collections`                    | List every non-deleted collection. |
//! | GET    | `/collections/{id}`               | Single collection. |
//! | GET    | `/collections/{id}/contents`      | Media in the collection. `?recursive=true` includes sub-folders. Authorized as `Read(CollectionContents { recursive })` — a media read, not a collection read. |
//! | DELETE | `/collections/{id}`               | Soft-delete a collection **and its descendants** (Media inside is orphaned, not deleted). Authorized as `Delete(CollectionSubtree)`, not `Delete(Collection)`. |
//! | POST   | `/tags`                           | Create / upsert: body `{slug}`. Authorized as `Add(NewTag)`. |
//! | GET    | `/tags`                           | All tags. |
//! | GET    | `/tags/popular`                   | Top tags by usage count. `?limit=N`. |
//! | GET    | `/tags/{slug}/media`              | Media carrying the tag. `?limit=N&offset=N`. |
//!
//! ## Quick start
//!
//! Implementing [`MediaAuthorizer`] needs the `async-trait` attribute.
//! The crate re-exports it as [`crate::media::async_trait`], so you do
//! not add the dependency yourself and versions cannot drift.
//!
//! ```ignore
//! use rustango::media::{MediaManager, router::media_router_with};
//! use rustango::storage::StorageRegistry;
//!
//! // Tables come from the framework's system migrations.
//!
//! // `new_pool` works on all three backends; `MediaManager::new` is
//! // Postgres-only and takes a `PgPool`.
//! let manager = MediaManager::new_pool(pool.clone(), registry);
//! let app = axum::Router::new()
//!     .nest("/media", media_router_with(manager, MyAuthorizer));
//! ```
//!
//! `MyAuthorizer` is yours — see [`MediaAuthorizer`]. With the
//! **`tenancy`** feature there is a shipped one:
//!
//! ```ignore
//! use rustango::media::router::{media_router_with, MediaPerms};
//!
//! let app = axum::Router::new()
//!     .nest("/media", media_router_with(manager, MediaPerms::new(pool)));
//! ```
//!
//! [`MediaPerms`] checks the `{table}.{action}` permission codenames
//! the admin already uses. It is table-level, not row-level — see its
//! docs for what that does and does not cover.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use crate::api_errors::ApiError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    Media, MediaCollection, MediaError, MediaManager, MediaStatus, MediaTag, UploadIntent,
};

#[allow(dead_code)]
const DEFAULT_PRESIGN_TTL_SECS: u64 = 3600;

/// What a request names — the row an authorizer is deciding about.
///
/// The kind is part of the value on purpose. `/media/7` and
/// `/collections/7` are different rows in different tables, so an
/// authorizer handed a bare `7` could not tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaTarget {
    /// One media row, by id.
    Media(i64),
    /// One collection row, by id — `GET /collections/{id}`.
    Collection(i64),
    /// The **media inside** one collection — `GET
    /// /collections/{id}/contents`, with or without `?recursive`.
    ///
    /// Separate from [`Self::Collection`] because this route returns
    /// media rows, each with a presigned GET URL. Granting it is a
    /// decision about media, not just about browsing folders. The id
    /// still names a collection, so a policy that scopes collections by
    /// owner can use it the same way it uses [`Self::Collection`].
    ///
    /// **`recursive` widens the same read.** It also returns the media
    /// in every descendant collection, so it is always more rows. Any
    /// policy must be at least as strict for `recursive: true` as for
    /// `recursive: false`.
    ///
    /// Match as `MediaTarget::CollectionContents { id, recursive }`, or
    /// `{ id, .. }` when the width does not change the answer.
    CollectionContents {
        /// The collection named in the path.
        id: i64,
        /// `true` when the request carries `?recursive`, and so reaches
        /// media in descendant collections as well.
        recursive: bool,
    },
    /// A collection **and everything under it** — what
    /// `DELETE /collections/{id}` really reaches.
    ///
    /// Deleting a collection soft-deletes every descendant collection
    /// and sets `collection_id = NULL` on the media in all of them. One
    /// authorized id can destroy a subtree of any size. The caller does
    /// not control that shape either: `POST /collections` takes
    /// `parent_id`, so anyone who may create a collection can attach one
    /// under someone else's.
    ///
    /// **Granting this is not the same as granting `Delete(Collection)`.**
    /// A policy that ends on `_ => false` denies it until someone opts
    /// in.
    CollectionSubtree(i64),
    /// One tag, by slug. A slug is caller-chosen text — including
    /// text that looks like a number — so it is never an id.
    Tag(String),
    /// `POST /uploads/begin`. Names no existing row: it mints a
    /// presigned `PUT` for a caller-chosen disk and key prefix.
    ///
    /// **Grant this narrowly.** It is a write primitive into storage.
    /// `/uploads/{id}/finalize` is *not* this — it mutates an existing
    /// row and classifies as `Change(Media(id))`.
    ///
    /// The fields are what the caller **asked for**, read from the body
    /// before the handler runs, so a policy can allow-list a disk, pin a
    /// key prefix per tenant, or refuse an upload attributed to someone
    /// else. They are **unvalidated input**, not facts: a body that does
    /// not parse arrives as empty strings and `None`, because
    /// authorization runs before validation. The handler rejects a bad
    /// body afterwards.
    ///
    /// Match it as `MediaTarget::NewUpload { disk, .. }`, with the
    /// trailing `..`.
    #[non_exhaustive]
    NewUpload {
        /// Requested `disk` — a key into the [`crate::storage::StorageRegistry`].
        disk: String,
        /// Requested `key_prefix`, prepended to the generated object key.
        key_prefix: String,
        /// Requested `collection_id`, or `None` for the library root.
        collection_id: Option<i64>,
        /// Requested `uploaded_by_id`. Attribution is caller-supplied on
        /// this route, so a policy that cares should check it against
        /// the authenticated principal rather than trust it.
        uploaded_by_id: Option<i64>,
    },
    /// `POST /collections`. Names no existing row: it creates one,
    /// optionally under a `parent_id` from the body.
    ///
    /// That `parent_id` is why it is its own decision. Collections
    /// nest, `DELETE /collections/{id}` takes a whole subtree, and
    /// anyone who may create a collection may attach one under someone
    /// else's — see [`Self::CollectionSubtree`].
    ///
    /// Match as `MediaTarget::NewCollection { .. }`, with the braces.
    #[non_exhaustive]
    NewCollection {},
    /// `POST /tags`. Creates (or upserts) a tag by slug.
    ///
    /// Separate from [`Self::NewCollection`]: different table,
    /// different decision. Match as `MediaTarget::NewTag { .. }`, with
    /// the braces.
    #[non_exhaustive]
    NewTag {},
    /// A read that enumerates rather than naming a row — `GET
    /// /collections`, `GET /tags` and `GET /tags/popular`.
    ///
    /// A `?recursive` contents listing is **not** one of these; it
    /// names a collection, so it keeps the id. See
    /// [`Self::CollectionContents`].
    ///
    /// **Granting this is not harmless.** Enumeration turns "guess an
    /// id" into "read the index".
    Listing,
}

/// What a request is trying to do, handed to a [`MediaAuthorizer`].
///
/// The four verbs line up with the `view` / `add` / `change` / `delete`
/// codenames used everywhere else in this codebase, so a policy built
/// on permissions maps onto them one-to-one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaAction {
    /// Read a row or a listing.
    Read(MediaTarget),
    /// Create: a collection, a tag, or an upload ticket.
    Add(MediaTarget),
    /// Mutate an existing row — move, (un)tag, finalize an upload.
    Change(MediaTarget),
    /// Destroy an existing row.
    Delete(MediaTarget),
}

/// What a [`MediaAuthorizer`] decided, and so what the client is told.
///
/// Three-valued rather than `bool`: "nobody is signed in" and "signed
/// in, but may not do this" are different answers. A token client
/// treats `401` as its cue to refresh, so answering `403` to an
/// anonymous request silently logs the member out.
///
/// `From<bool>` is implemented, so a policy that already computes a
/// boolean can `return allowed.into()`; `false` becomes
/// [`Self::Forbidden`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaDecision {
    /// Authorized — run the handler.
    Allow,
    /// No authenticated principal. The client is told `401` so it knows
    /// to authenticate.
    ///
    /// Return this only when there is no identity at all. A signed-in
    /// user who lacks the permission is [`Self::Forbidden`]; answering
    /// `401` there sends a token client into a refresh loop that cannot
    /// succeed.
    Unauthenticated,
    /// There is a principal (or the policy does not care), and the
    /// answer is no. `403`.
    Forbidden,
}

impl From<bool> for MediaDecision {
    /// `true` → [`MediaDecision::Allow`], `false` →
    /// [`MediaDecision::Forbidden`].
    ///
    /// Never `Unauthenticated`: a bare `false` says nothing about
    /// whether a principal existed.
    fn from(allowed: bool) -> Self {
        if allowed {
            Self::Allow
        } else {
            Self::Forbidden
        }
    }
}

/// Decides whether a request may touch the media surface.
///
/// **There is no default implementation, on purpose.** Without a
/// policy these 16 routes take no identity and no tenant, so an
/// anonymous caller could harvest presigned S3 links, delete by id,
/// and mint a presigned PUT for any key prefix.
///
/// Implement it against whatever your app uses for identity, and scope
/// by tenant here if you are multi-tenant — [`MediaTarget`] names the
/// row for exactly that.
///
/// ```ignore
/// struct SessionAuthorizer;
///
/// // Re-exported by the crate — no `async-trait` dependency of your own.
/// #[rustango::media::async_trait]
/// impl MediaAuthorizer for SessionAuthorizer {
///     async fn authorize(&self, parts: &Parts, action: MediaAction) -> MediaDecision {
///         // No identity at all is 401, not 403 — the client is being
///         // told to authenticate, not that it may never do this.
///         let Some(user) = current_user(parts) else {
///             return MediaDecision::Unauthenticated;
///         };
///         let allowed = match action {
///             MediaAction::Read(t) => match t {
///                 MediaTarget::Media(id) => user.may_read_media(id).await,
///                 MediaTarget::Collection(id) => user.may_read_collection(id).await,
///                 MediaTarget::Tag(slug) => user.may_read_tag(&slug).await,
///                 // Listings enumerate the tenant's whole library, so
///                 // they are an explicit decision, not a default.
///                 MediaTarget::Listing => user.may_browse_library(),
///                 _ => false,
///             },
///             // `NewUpload` mints a presigned PUT for a disk and key
///             // prefix the *caller* chooses — and hands you both, so
///             // the grant can be "this disk, under your own prefix"
///             // rather than "anywhere in any bucket".
///             MediaAction::Add(MediaTarget::NewUpload { disk, key_prefix, .. }) => {
///                 user.is_trusted_uploader()
///                     && disk == "user-uploads"
///                     && key_prefix.starts_with(&user.prefix())
///             }
///             MediaAction::Add(MediaTarget::NewCollection { .. }) => user.is_editor(),
///             MediaAction::Add(MediaTarget::NewTag { .. }) => user.is_editor(),
///             MediaAction::Change(MediaTarget::Media(id)) => user.owns_media(id).await,
///             MediaAction::Delete(MediaTarget::Media(id)) => user.owns_media(id).await,
///             // Deleting a collection takes its whole subtree and
///             // orphans the media at every level, so it is a separate
///             // decision from deleting one row. Left to `_ => false`
///             // here: opt in only where you mean it.
///             _ => false,
///         };
///         allowed.into()
///     }
/// }
/// ```
///
/// Note the trailing `_ => false` arms. [`MediaAction`] and
/// [`MediaTarget`] are both `#[non_exhaustive]`, so a future route
/// reaches your policy as a variant you have no arm for. Ending on
/// `false` denies it.
///
/// # Your impl runs on every request, and nothing bounds it
///
/// - **There is no timeout here.** A policy that hangs pins its
///   request. The gate touches no pool, but a policy that queries holds
///   a connection from its own pool while it hangs. Mount
///   [`crate::request_timeout`] on the outer router for a bound.
/// - **Do not hold a lock across the `.await`.** "Check a cache, else
///   hit the database" compiles, but a `tokio::sync::Mutex` held across
///   the await serializes the whole media surface. Take the lock, read,
///   drop it, then await.
#[async_trait::async_trait]
pub trait MediaAuthorizer: Send + Sync + 'static {
    /// [`MediaDecision::Allow`] to run the handler;
    /// [`MediaDecision::Unauthenticated`] for `401`;
    /// [`MediaDecision::Forbidden`] for `403`.
    ///
    /// A policy that already computes a boolean can return
    /// `allowed.into()`; `false` is `Forbidden`.
    async fn authorize(
        &self,
        parts: &axum::http::request::Parts,
        action: MediaAction,
    ) -> MediaDecision;
}

/// So a host that picks its policy at runtime can pass
/// `Arc<dyn MediaAuthorizer>`, which the bare generic bound rejects.
#[async_trait::async_trait]
impl<T: MediaAuthorizer + ?Sized> MediaAuthorizer for Arc<T> {
    async fn authorize(
        &self,
        parts: &axum::http::request::Parts,
        action: MediaAction,
    ) -> MediaDecision {
        (**self).authorize(parts, action).await
    }
}

/// Refuses everything. What [`media_router`] uses.
struct DenyAll;

#[async_trait::async_trait]
impl MediaAuthorizer for DenyAll {
    async fn authorize(&self, _: &axum::http::request::Parts, _: MediaAction) -> MediaDecision {
        // `Forbidden`, not `Unauthenticated`: it refuses whoever asks,
        // so telling a client to authenticate would invite a retry loop.
        MediaDecision::Forbidden
    }
}

/// The permission codenames a [`MediaPerms`] request must satisfy —
/// **all** of them, not any.
///
/// `None` means "no mapping", which [`MediaPerms`] treats as a
/// refusal. A future `MediaTarget` lands there, so a new route cannot
/// inherit a grant written before it existed.
///
/// Public and free-standing so the mapping is testable without
/// standing a router up.
#[cfg(feature = "tenancy")]
#[must_use]
pub fn required_codenames(action: &MediaAction) -> Option<&'static [&'static str]> {
    use MediaAction as A;
    use MediaTarget as T;
    Some(match action {
        // `GET /tags/{slug}/media` returns **media** rows, so it is a
        // media read. The tag is the filter, not the subject.
        A::Read(T::Media(_) | T::Tag(_)) => &["rustango_media.view"],
        A::Read(T::Collection(_)) => &["rustango_media_collections.view"],
        // Contents returns media rows, so it takes the media permission
        // — plus the collection one, since the id names a collection
        // the caller must be allowed to open. Both widths take the same
        // two; `recursive` is on the variant so a custom policy can be
        // stricter about the wide one, never looser.
        A::Read(T::CollectionContents { .. }) => {
            &["rustango_media_collections.view", "rustango_media.view"]
        }
        // `Listing` spans three tables, so it takes the widest
        // permission of the group. Kept separate from the `Media | Tag`
        // arm: same codename, different reason.
        #[allow(clippy::match_same_arms)]
        A::Read(T::Listing) => &["rustango_media.view"],
        A::Add(T::NewUpload { .. }) => &["rustango_media.add"],
        A::Add(T::NewCollection { .. }) => &["rustango_media_collections.add"],
        A::Add(T::NewTag { .. }) => &["rustango_media_tags.add"],
        A::Change(T::Media(_)) => &["rustango_media.change"],
        A::Delete(T::Media(_)) => &["rustango_media.delete"],
        // Both are needed. Deleting a collection soft-deletes every
        // descendant *and* sets `collection_id = NULL` on the media in
        // all of them, so it writes to `rustango_media` too.
        A::Delete(T::CollectionSubtree(_)) => {
            &["rustango_media_collections.delete", "rustango_media.change"]
        }
        _ => return None,
    })
}

/// The default policy: permission codenames, the same
/// `{table}.{action}` names the admin and `auto_create_permissions`
/// already use.
///
/// This exists so the **secure** path is the one-liner. Do not reach
/// for an `AllowAll` trait impl to clear the 403s `media_router` gives
/// you — that is the original hole with extra steps.
///
/// ```ignore
/// use rustango::media::router::{media_router_with, MediaPerms};
///
/// let app = axum::Router::new()
///     .nest("/media", media_router_with(manager, MediaPerms::new(pool)));
/// ```
///
/// Mount it **inside** [`crate::tenancy::middleware::RouterAuthExt::require_auth`],
/// which injects the `AuthenticatedUser` this reads. Without it every
/// request is [`MediaDecision::Unauthenticated`] — a `401`.
///
/// **Not `optional_auth`.** This policy has no anonymous path, so it
/// still answers `401` to every anonymous request. `optional_auth`
/// only helps under a **custom** [`MediaAuthorizer`] that allows some
/// anonymous action. For a public *page*, do not mount this router at
/// all (see the module header).
///
/// # What it checks
///
/// [`required_codenames`] has the full mapping. Superusers
/// short-circuit to allow, except for [`Self::allow_disks`], which is
/// checked first and applies to them too. `is_superuser` is
/// per-tenant, and that disk list is the only thing keeping one
/// tenant's admin out of another tenant's bucket.
///
/// # What it cannot check
///
/// **Codenames are table-level, so this is not row-level.** A grant of
/// `rustango_media.view` reads *any* media row by id. `MediaManager`
/// holds a single [`crate::sql::Pool`], so a multi-tenant deployment
/// must still scope rows itself with its own [`MediaAuthorizer`]. This
/// type is the floor, not the ceiling.
///
/// `auto_create_permissions` seeds the codenames during tenant
/// provisioning and on migrate. To re-seed without a migrate cycle,
/// use the `seed-permissions` manage command; it is a no-op on an
/// already-populated catalog.
///
/// # Cost
///
/// One indexed permission lookup per required codename, per request,
/// on the pool given to [`Self::new`]. A superuser pays none.
#[cfg(feature = "tenancy")]
pub struct MediaPerms {
    pool: crate::sql::Pool,
    allowed_disks: Option<Vec<String>>,
}

#[cfg(feature = "tenancy")]
impl MediaPerms {
    /// Check permissions against `pool`.
    ///
    /// Under tenancy pass the tenant's pool, not the registry's. It is
    /// where `rustango_user_permissions` and `rustango_user_roles` are
    /// read, so the wrong pool reads the wrong tenant's grants.
    ///
    /// Until you call [`Self::allow_disks`], anyone holding
    /// `rustango_media.add` can write to every disk in the
    /// [`crate::storage::StorageRegistry`].
    #[must_use]
    pub fn new(pool: crate::sql::Pool) -> Self {
        Self {
            pool,
            allowed_disks: None,
        }
    }

    /// Restrict `POST /uploads/begin` to these disks.
    ///
    /// `disk` is caller-supplied and goes straight into
    /// `StorageRegistry::disk`, and the registry is **process-wide**:
    /// pool-per-tenant isolates the database, not the object store. So
    /// a bare `rustango_media.add` grant mints a presigned `PUT` into
    /// *any* registered bucket. A codename cannot say "this disk",
    /// which is why this is a list. It is checked **in addition to**
    /// `rustango_media.add`, never instead of it.
    ///
    /// **It also binds superusers**, alone among the checks here.
    /// `is_superuser` elevates inside one tenant; this list is what
    /// stops that reaching another tenant's storage. Leave it unset if
    /// org admins should be exempt.
    ///
    /// ```ignore
    /// MediaPerms::new(pool).allow_disks(["user-uploads"])
    /// ```
    ///
    /// Prefixes within a disk are not expressible here. For "your own
    /// prefix on a shared bucket", implement [`MediaAuthorizer`], which
    /// is handed `key_prefix`.
    #[must_use]
    pub fn allow_disks<I, S>(mut self, disks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_disks = Some(disks.into_iter().map(Into::into).collect());
        self
    }

    /// `true` when `disk` may be written to. Unset means every disk —
    /// see [`Self::allow_disks`].
    fn disk_allowed(&self, disk: &str) -> bool {
        match &self.allowed_disks {
            Some(allowed) => allowed.iter().any(|d| d == disk),
            None => true,
        }
    }
}

#[cfg(feature = "tenancy")]
#[async_trait::async_trait]
impl MediaAuthorizer for MediaPerms {
    async fn authorize(
        &self,
        parts: &axum::http::request::Parts,
        action: MediaAction,
    ) -> MediaDecision {
        let Some(auth) = parts
            .extensions
            .get::<crate::tenancy::middleware::AuthenticatedUser>()
        else {
            // No principal at all — 401, so a token client refreshes
            // instead of treating the refusal as final. It also names
            // the mistake of mounting outside `require_auth`.
            return MediaDecision::Unauthenticated;
        };
        // The disk allow-list runs **ahead of the superuser
        // short-circuit**, and it is the only check that does.
        // `is_superuser` is per-tenant, and `StorageRegistry` is
        // process-wide, so short-circuiting first would let any
        // tenant's admin mint a presigned PUT into another tenant's
        // bucket. It is applied on top of the codename, never instead
        // of it. Leave `allow_disks` unset to exempt admins.
        if let MediaAction::Add(MediaTarget::NewUpload { disk, .. }) = &action {
            if !self.disk_allowed(disk) {
                tracing::debug!(
                    target: "rustango::media::auth",
                    disk = %disk,
                    user_id = auth.id,
                    superuser = auth.is_superuser,
                    "MediaPerms: upload ticket refused — disk not in the allow-list"
                );
                return MediaDecision::Forbidden;
            }
        }
        if auth.is_superuser {
            return MediaDecision::Allow;
        }
        // A variant with no mapping is a route added after this policy
        // was written. Denying it is the point.
        let Some(codenames) = required_codenames(&action) else {
            tracing::debug!(
                target: "rustango::media::auth",
                ?action,
                "MediaPerms has no codename for this action — refusing"
            );
            return MediaDecision::Forbidden;
        };
        // An empty slice would fall straight through the loop to
        // `Allow`. A mapping that requires nothing is a mistake, not a
        // grant.
        if codenames.is_empty() {
            tracing::warn!(
                target: "rustango::media::auth",
                ?action,
                "MediaPerms: empty codename list — refusing rather than allowing"
            );
            return MediaDecision::Forbidden;
        }
        for cn in codenames {
            match crate::tenancy::permissions::has_perm_pool(auth.id, cn, &self.pool).await {
                Ok(true) => {}
                Ok(false) => return MediaDecision::Forbidden,
                Err(e) => {
                    // Fail closed. A driver error is not a grant.
                    tracing::warn!(
                        target: "rustango::media::auth",
                        codename = %cn,
                        user_id = auth.id,
                        error = %e,
                        "permission lookup failed — refusing"
                    );
                    return MediaDecision::Forbidden;
                }
            }
        }
        MediaDecision::Allow
    }
}

/// The slice of `POST /uploads/begin`'s body the gate reads, so the
/// policy can see what the caller asked for.
///
/// Every field defaults, and a body that does not parse falls back to
/// the struct's default. That is on purpose: the gate must not answer
/// `400`. Authorization is decided first, on whatever the caller sent;
/// the handler validates afterwards.
#[derive(Debug, Default, Deserialize)]
struct UploadRequestDetail {
    #[serde(default)]
    disk: String,
    #[serde(default)]
    key_prefix: String,
    #[serde(default)]
    collection_id: Option<i64>,
    #[serde(default)]
    uploaded_by_id: Option<i64>,
}

/// The most the gate will buffer from a body it has to read.
///
/// An upload-ticket body is a few hundred bytes of JSON. 16 KiB is far
/// above any real one and small enough that buffering it in an
/// authorization layer is not a denial-of-service primitive. Only
/// [`wants_upload_body`] routes are read; the rest stream through.
const MAX_GATE_BODY_BYTES: usize = 16 * 1024;

/// Does this request's body have to be read before it can be
/// classified?
///
/// True for `POST /uploads/begin` alone. That route takes a
/// caller-chosen disk and key prefix in the body, and the policy needs
/// both.
///
/// Its own function because the middleware must decide whether to
/// buffer *before* it can call [`classify`].
fn wants_upload_body(method: &axum::http::Method, path: &str) -> bool {
    let seg: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(crate::url_codec::percent_decode_path)
        .collect();
    method == axum::http::Method::POST && seg.len() == 2 && seg[0] == "uploads" && seg[1] == "begin"
}

/// Classify a request so the authorizer sees what it is deciding about.
///
/// `None` means the request matches no route in this table, and the
/// caller must refuse. Fail closed.
///
/// Segments are percent-decoded, then matched **positionally** against
/// the route table. Both matter: scanning for the first
/// integer-parsable segment of a raw path misses `/media/%31` and
/// mistakes the `2024` in `/tags/2024/media` for an id.
///
/// `query` is read for one thing: `?recursive` on a collection's
/// contents reaches media in descendant collections. That widening
/// rides on [`MediaTarget::CollectionContents`] instead of replacing
/// the target, so the id survives and no policy can ask less of the
/// wide form than of the narrow one.
///
/// `upload` carries the parsed body for the one route that needs it —
/// see [`wants_upload_body`]. A `None` on `POST /uploads/begin` gives
/// a target with empty fields, not no target: an unreadable body is
/// still an upload request, and the policy should refuse it.
fn classify(
    method: &axum::http::Method,
    path: &str,
    query: Option<&str>,
    upload: Option<UploadRequestDetail>,
) -> Option<MediaAction> {
    use axum::http::Method;

    let seg: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(crate::url_codec::percent_decode_path)
        .collect();
    let s: Vec<&str> = seg.iter().map(String::as_str).collect();

    // An id that does not parse cannot name a row, so there is nothing
    // to authorize — refuse rather than guess a target.
    let id = |raw: &str| raw.parse::<i64>().ok();

    // axum routes HEAD to the GET handler, so the gate has to agree or
    // a documented GET route is unusable with HEAD.
    let m = if method == Method::HEAD {
        &Method::GET
    } else {
        method
    };

    // Presence, not value: naming the parameter at all is a request to
    // widen, and the handler rejects a non-boolean itself. Being
    // stricter than the handler is the safe direction here.
    //
    // The key is **decoded first**, with form semantics, to match the
    // handler's `Query` extractor: `serde_urlencoded` parses through
    // `form_urlencoded`, which percent-decodes the key and turns `+`
    // into a space. Matching raw bytes let `?%72ecursive=true` reach
    // the handler as a subtree walk while the gate saw a single read.
    let recursive = query.is_some_and(|q| {
        q.split('&').any(|p| {
            let key = p.split('=').next().unwrap_or(p);
            crate::url_codec::url_decode(key) == "recursive"
        })
    });

    match (m, s.as_slice()) {
        (&Method::POST, ["uploads", "begin"]) => {
            let d = upload.unwrap_or_default();
            Some(MediaAction::Add(MediaTarget::NewUpload {
                disk: d.disk,
                key_prefix: d.key_prefix,
                collection_id: d.collection_id,
                uploaded_by_id: d.uploaded_by_id,
            }))
        }
        // Finalize mutates the media row it names, so it is a `Media`
        // target rather than a kind of its own.
        (&Method::POST, ["uploads", raw, "finalize"]) => {
            Some(MediaAction::Change(MediaTarget::Media(id(raw)?)))
        }

        (&Method::GET, ["media", raw]) => Some(MediaAction::Read(MediaTarget::Media(id(raw)?))),
        (&Method::DELETE, ["media", raw]) => {
            Some(MediaAction::Delete(MediaTarget::Media(id(raw)?)))
        }
        // move / tag / untag all mutate the media row itself.
        (&Method::POST, ["media", raw, "move" | "tags"])
        | (&Method::DELETE, ["media", raw, "tags", _]) => {
            Some(MediaAction::Change(MediaTarget::Media(id(raw)?)))
        }

        (&Method::GET, ["collections"]) => Some(MediaAction::Read(MediaTarget::Listing)),
        (&Method::POST, ["collections"]) => Some(MediaAction::Add(MediaTarget::NewCollection {})),
        // The collection row itself.
        (&Method::GET, ["collections", raw]) => {
            Some(MediaAction::Read(MediaTarget::Collection(id(raw)?)))
        }
        // The media inside it — a different table, and a presigned URL
        // per row, so it cannot share `Collection` with the arm above.
        // One arm for both widths: `?recursive` rides on the target as
        // a flag so the id survives for a row-scoping policy.
        (&Method::GET, ["collections", raw, "contents"]) => {
            Some(MediaAction::Read(MediaTarget::CollectionContents {
                id: id(raw)?,
                recursive,
            }))
        }
        // Not `Collection` — this route deletes the whole subtree and
        // orphans the media at every level of it.
        (&Method::DELETE, ["collections", raw]) => Some(MediaAction::Delete(
            MediaTarget::CollectionSubtree(id(raw)?),
        )),

        // `popular` is a listing, and must be matched before the
        // `{slug}` arm or a tag literally named "popular" shadows it.
        (&Method::GET, ["tags"] | ["tags", "popular"]) => {
            Some(MediaAction::Read(MediaTarget::Listing))
        }
        (&Method::POST, ["tags"]) => Some(MediaAction::Add(MediaTarget::NewTag {})),
        (&Method::GET, ["tags", slug, "media"]) => {
            Some(MediaAction::Read(MediaTarget::Tag((*slug).to_owned())))
        }

        _ => None,
    }
}

/// Build the media router with an authorization policy.
///
/// Every route is gated: the authorizer runs before the handler, so a
/// refusal costs no database work and never reaches the presigning
/// code.
pub fn media_router_with<A: MediaAuthorizer>(manager: MediaManager, authorizer: A) -> Router {
    let auth = Arc::new(authorizer);
    media_routes(manager)
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let auth = Arc::clone(&auth);
                async move {
                    let (parts, body) = req.into_parts();
                    // One route needs its body to be classified: the one
                    // that mints a presigned PUT. Buffered under a cap
                    // and handed straight back, so nothing downstream
                    // notices — except that the policy now knows which
                    // disk and prefix it is being asked to allow.
                    let (upload, body) = if wants_upload_body(&parts.method, parts.uri.path()) {
                        // Over the cap, or the stream failed. Refusing
                        // keeps the gate from being a place to make a
                        // server buffer.
                        let Ok(bytes) = axum::body::to_bytes(body, MAX_GATE_BODY_BYTES).await
                        else {
                            tracing::debug!(
                                target: "rustango::media::auth",
                                path = %parts.uri.path(),
                                "refused: upload request body could not be read within \
                                 the gate's limit"
                            );
                            return (
                                StatusCode::FORBIDDEN,
                                Json(serde_json::json!({
                                    "error": "upload request body too large to authorize"
                                })),
                            )
                                .into_response();
                        };
                        // A body that does not parse is still an upload
                        // request. It reaches the policy with empty
                        // fields, not as a 400: the gate decides
                        // authorization, the handler decides validity.
                        let detail = serde_json::from_slice::<UploadRequestDetail>(&bytes)
                            .unwrap_or_default();
                        (Some(detail), axum::body::Body::from(bytes))
                    } else {
                        (None, body)
                    };
                    // No classification means no route here matches, or
                    // the id cannot be a row id. Refuse rather than
                    // pass an unrecognised shape through.
                    let Some(action) =
                        classify(&parts.method, parts.uri.path(), parts.uri.query(), upload)
                    else {
                        tracing::debug!(
                            target: "rustango::media::auth",
                            method = %parts.method,
                            path = %parts.uri.path(),
                            "refused: no route in the media table matches"
                        );
                        return ApiError::forbidden("not a recognised media operation")
                            .into_response();
                    };
                    let decision = auth.authorize(&parts, action.clone()).await;
                    if decision != MediaDecision::Allow {
                        // `debug`, so it costs nothing in production, but
                        // a subtly-wrong policy stays debuggable.
                        tracing::debug!(
                            target: "rustango::media::auth",
                            ?action,
                            ?decision,
                            method = %parts.method,
                            path = %parts.uri.path(),
                            "refused by the authorization policy"
                        );
                        // 401 means "authenticate", 403 means "you may
                        // not". A token client treats 401 as its cue to
                        // refresh.
                        return match decision {
                            MediaDecision::Unauthenticated => {
                                ApiError::unauthorized("authentication required")
                            }
                            _ => ApiError::forbidden("not authorized for this media operation"),
                        }
                        .into_response();
                    }
                    next.run(axum::extract::Request::from_parts(parts, body))
                        .await
                }
            },
        ))
        // Outermost, so it also covers the gate's own early 403s: the
        // last `.layer` wraps the ones before it.
        .layer(axum::middleware::from_fn(no_store))
}

/// `Cache-Control: no-store` and `Vary` on every response.
///
/// These routes hand out **presigned URLs** — short-lived bearer
/// credentials that work for anyone holding them until the TTL
/// expires. Without a directive, a 200 `GET` with no explicit
/// freshness is heuristically cacheable (RFC 9111 §4.2.2), and with no
/// `Vary` the cache key is just method plus URI.
///
/// RFC 9111 §3.5 keeps a shared cache off a response whose request
/// carried `Authorization`, but says **nothing about `Cookie`** — and
/// the authorizer this module documents is cookie/session shaped. So a
/// CDN could store one user's signed link and serve it to the next
/// caller of the same URI.
///
/// Applied to the whole surface, not just the routes that embed a
/// signature today, so a new route cannot opt out by accident.
/// `no-store` rather than `private`, because `private` still lets a
/// browser keep the credential on disk.
///
/// # Scope
///
/// This covers the JSON **carrying** a presigned URL, not the later
/// fetch of the object through that URL — a different request to a
/// different origin, under its own cache rules.
///
/// Mounted with `nest`, an unmatched path under the prefix is answered
/// by the *outer* router's fallback and carries neither header.
/// Nothing is served there, so no credential leaks. Adding a
/// `.fallback()` here would close that, but `Router::merge` does not
/// reject two fallbacks, so an integrator who merges would silently
/// get 403 for every unknown URL in their app.
async fn no_store(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    // `insert`, not `append`. A handler that set its own `Cache-Control`
    // would otherwise leave two directives on the response, and the
    // weaker one could win.
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    // `Vary` as well, because it works differently: `no-store` asks a
    // cache not to store, while `Vary` changes the cache key. CDNs
    // routinely override origin directives (nginx
    // `proxy_ignore_headers`, Cloudflare "Cache Everything", Fastly
    // VCL), and each of those drops `no-store`. Ignoring `Vary` takes
    // a separate, deliberate step. Losing hit rate here is the point.
    //
    // Both header names, because the authorizer reads `Parts` and may
    // key off either.
    resp.headers_mut().insert(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static("Cookie, Authorization"),
    );
    resp
}

/// Build the media router. Pass to `axum::Router::nest("/media", ...)`.
///
/// # This refuses every request
///
/// It mounts a refuse-everything policy, so every route answers `403`.
/// Use [`media_router_with`] and supply a [`MediaAuthorizer`].
#[deprecated(
    since = "0.57.7",
    note = "serves nothing — every route is 403, and it is removed in 0.59.0. Use `media_router_with(manager, authorizer)` and supply a `MediaAuthorizer`; see the module docs."
)]
pub fn media_router(manager: MediaManager) -> Router {
    media_router_with(manager, DenyAll)
}

fn media_routes(manager: MediaManager) -> Router {
    let state = Arc::new(manager);
    Router::new()
        .route("/uploads/begin", post(begin_upload_handler))
        .route("/uploads/{id}/finalize", post(finalize_upload_handler))
        .route(
            "/media/{id}",
            get(get_media_handler).delete(delete_media_handler),
        )
        .route("/media/{id}/move", post(move_media_handler))
        .route("/media/{id}/tags", post(set_tags_handler))
        .route("/media/{id}/tags/{slug}", delete(untag_handler))
        .route(
            "/collections",
            post(create_collection_handler).get(list_collections_handler),
        )
        .route(
            "/collections/{id}",
            get(get_collection_handler).delete(delete_collection_handler),
        )
        .route(
            "/collections/{id}/contents",
            get(collection_contents_handler),
        )
        .route("/tags", post(create_tag_handler).get(list_tags_handler))
        .route("/tags/popular", get(popular_tags_handler))
        .route("/tags/{slug}/media", get(media_with_tag_handler))
        .with_state(state)
}

// =====================================================================
// Wire types
// =====================================================================

#[derive(Debug, Deserialize)]
struct BeginUploadBody {
    disk: String,
    #[serde(default)]
    key_prefix: String,
    mime: String,
    original_filename: String,
    size_bytes: i64,
    #[serde(default)]
    uploaded_by_id: Option<i64>,
    #[serde(default)]
    collection_id: Option<i64>,
    #[serde(default = "default_ttl_secs")]
    ttl_secs: u64,
}

fn default_ttl_secs() -> u64 {
    300
}

#[derive(Debug, Serialize)]
struct UploadTicketBody {
    media_id: i64,
    upload_url: String,
    expires_at: chrono::DateTime<chrono::Utc>,
    disk: String,
    storage_key: String,
}

#[derive(Debug, Serialize)]
struct MediaResponse {
    id: i64,
    disk: String,
    storage_key: String,
    mime: String,
    size_bytes: i64,
    original_filename: String,
    status: String,
    uploaded_at: chrono::DateTime<chrono::Utc>,
    uploaded_by_id: Option<i64>,
    derived_from_id: Option<i64>,
    collection_id: Option<i64>,
    metadata: Value,
    /// CDN-aware public URL when available.
    url: Option<String>,
    /// Time-limited GET URL (1h by default), if the backend signs.
    presigned_url: Option<String>,
    tags: Vec<String>,
}

impl MediaResponse {
    async fn from_row(manager: &MediaManager, m: Media) -> Result<Self, MediaError> {
        let id = match m.id {
            crate::sql::Auto::Set(v) => v,
            _ => 0,
        };
        let tags = manager
            .tags_for(id)
            .await?
            .into_iter()
            .map(|t| t.slug)
            .collect();
        Ok(Self::from_row_with_tags(manager, m, tags).await)
    }

    /// [`Self::from_row`] with the tag slugs already in hand.
    ///
    /// Lets a listing fetch tags for the whole page in one query
    /// instead of one per row. Infallible, because the only fallible
    /// step in `from_row` was that per-row tag query.
    async fn from_row_with_tags(manager: &MediaManager, m: Media, tags: Vec<String>) -> Self {
        let id = match m.id {
            crate::sql::Auto::Set(v) => v,
            _ => 0,
        };
        let url = manager.url(&m);
        let presigned = manager
            .presigned_get(&m, Duration::from_secs(DEFAULT_PRESIGN_TTL_SECS))
            .await;
        Self {
            id,
            disk: m.disk,
            storage_key: m.storage_key,
            mime: m.mime,
            size_bytes: m.size_bytes,
            original_filename: m.original_filename,
            status: m.status,
            uploaded_at: m.uploaded_at.into_inner().unwrap_or_else(chrono::Utc::now),
            uploaded_by_id: m.uploaded_by_id,
            derived_from_id: m.derived_from_id,
            collection_id: m.collection_id,
            metadata: m.metadata,
            url,
            presigned_url: presigned,
            tags,
        }
    }
}

#[derive(Debug, Deserialize)]
struct MoveBody {
    collection_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SetTagsBody {
    slugs: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CreateCollectionBody {
    name: String,
    slug: String,
    #[serde(default)]
    parent_id: Option<i64>,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Serialize)]
struct CollectionResponse {
    id: i64,
    name: String,
    slug: String,
    parent_id: Option<i64>,
    description: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<MediaCollection> for CollectionResponse {
    fn from(c: MediaCollection) -> Self {
        let id = match c.id {
            crate::sql::Auto::Set(v) => v,
            _ => 0,
        };
        Self {
            id,
            name: c.name,
            slug: c.slug,
            parent_id: c.parent_id,
            description: c.description,
            created_at: c.created_at.into_inner().unwrap_or_else(chrono::Utc::now),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateTagBody {
    slug: String,
}

#[derive(Debug, Serialize)]
struct TagResponse {
    id: i64,
    name: String,
    slug: String,
}

impl From<MediaTag> for TagResponse {
    fn from(t: MediaTag) -> Self {
        let id = match t.id {
            crate::sql::Auto::Set(v) => v,
            _ => 0,
        };
        Self {
            id,
            name: t.name,
            slug: t.slug,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PopularQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Debug, Deserialize)]
struct ListWithTagQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Debug, Deserialize)]
struct ContentsQuery {
    #[serde(default)]
    recursive: bool,
    /// Clamped server-side to `1..=1000`; absent means the default page.
    limit: Option<i64>,
    #[serde(default)]
    offset: i64,
}

// =====================================================================
// Error mapping
// =====================================================================

impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        let status = match &self {
            MediaError::UnknownDisk(_) => StatusCode::BAD_REQUEST,
            MediaError::Storage(_) => StatusCode::BAD_GATEWAY,
            MediaError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
            MediaError::Other(m) if m.contains("not found") => StatusCode::NOT_FOUND,
            MediaError::Other(_) => StatusCode::BAD_REQUEST,
        };
        ApiError::logged(status, &self).into_response()
    }
}

// =====================================================================
// Handlers
// =====================================================================

async fn begin_upload_handler(
    State(manager): State<Arc<MediaManager>>,
    Json(body): Json<BeginUploadBody>,
) -> Result<Json<UploadTicketBody>, MediaError> {
    let ticket = manager
        .begin_upload(UploadIntent {
            disk: body.disk,
            key_prefix: body.key_prefix,
            mime: body.mime,
            original_filename: body.original_filename,
            size_bytes: body.size_bytes,
            uploaded_by_id: body.uploaded_by_id,
            collection_id: body.collection_id,
            ttl: Duration::from_secs(body.ttl_secs.clamp(60, 3600)),
        })
        .await?;
    Ok(Json(UploadTicketBody {
        media_id: ticket.media_id,
        upload_url: ticket.upload_url,
        expires_at: ticket.expires_at,
        disk: ticket.disk,
        storage_key: ticket.storage_key,
    }))
}

async fn finalize_upload_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
) -> Result<Json<MediaResponse>, MediaError> {
    let m = manager.finalize_upload(id).await?;
    let resp = MediaResponse::from_row(&manager, m).await?;
    Ok(Json(resp))
}

async fn get_media_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
) -> Result<Json<MediaResponse>, MediaError> {
    let m = manager
        .get(id)
        .await?
        .ok_or_else(|| MediaError::Other(format!("media {id} not found")))?;
    let resp = MediaResponse::from_row(&manager, m).await?;
    Ok(Json(resp))
}

async fn delete_media_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, MediaError> {
    let m = manager
        .get(id)
        .await?
        .ok_or_else(|| MediaError::Other(format!("media {id} not found")))?;
    manager.delete(&m).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn move_media_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
    Json(body): Json<MoveBody>,
) -> Result<StatusCode, MediaError> {
    manager.move_to_collection(id, body.collection_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_tags_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
    Json(body): Json<SetTagsBody>,
) -> Result<StatusCode, MediaError> {
    let slugs: Vec<&str> = body.slugs.iter().map(String::as_str).collect();
    manager.set_tags(id, &slugs).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn untag_handler(
    State(manager): State<Arc<MediaManager>>,
    Path((id, slug)): Path<(i64, String)>,
) -> Result<StatusCode, MediaError> {
    manager.untag(id, &slug).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_collection_handler(
    State(manager): State<Arc<MediaManager>>,
    Json(body): Json<CreateCollectionBody>,
) -> Result<(StatusCode, Json<CollectionResponse>), MediaError> {
    let c = manager
        .create_collection(body.name, body.slug, body.parent_id, body.description)
        .await?;
    Ok((StatusCode::CREATED, Json(c.into())))
}

async fn list_collections_handler(
    State(manager): State<Arc<MediaManager>>,
) -> Result<Json<Vec<CollectionResponse>>, MediaError> {
    let cs = manager.list_collections().await?;
    Ok(Json(cs.into_iter().map(Into::into).collect()))
}

async fn get_collection_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
) -> Result<Json<CollectionResponse>, MediaError> {
    let c = manager
        .get_collection(id)
        .await?
        .ok_or_else(|| MediaError::Other(format!("collection {id} not found")))?;
    Ok(Json(c.into()))
}

async fn delete_collection_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, MediaError> {
    manager.delete_collection(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn collection_contents_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(id): Path<i64>,
    Query(q): Query<ContentsQuery>,
) -> Result<Json<Vec<MediaResponse>>, MediaError> {
    let media = match q.limit {
        Some(n) => {
            manager
                .list_in_collection_paged(id, q.recursive, n, q.offset)
                .await?
        }
        None => manager.list_in_collection(id, q.recursive).await?,
    };
    // One tag query for the whole page, not one per row.
    let ids: Vec<i64> = media
        .iter()
        .filter_map(|m| match m.id {
            crate::sql::Auto::Set(v) => Some(v),
            _ => None,
        })
        .collect();
    let mut tags = manager.tags_for_many(&ids).await?;
    let mut out = Vec::with_capacity(media.len());
    for m in media {
        let id = match m.id {
            crate::sql::Auto::Set(v) => v,
            _ => 0,
        };
        let row_tags = tags.remove(&id).unwrap_or_default();
        out.push(MediaResponse::from_row_with_tags(&manager, m, row_tags).await);
    }
    Ok(Json(out))
}

async fn create_tag_handler(
    State(manager): State<Arc<MediaManager>>,
    Json(body): Json<CreateTagBody>,
) -> Result<(StatusCode, Json<TagResponse>), MediaError> {
    let t = manager.ensure_tag(&body.slug).await?;
    Ok((StatusCode::CREATED, Json(t.into())))
}

async fn list_tags_handler(
    State(manager): State<Arc<MediaManager>>,
) -> Result<Json<Vec<TagResponse>>, MediaError> {
    // The manager has no `list_all_tags`; `popular_tags` with a high
    // limit covers the same ground, sorted by usage.
    let pairs = manager.popular_tags(1000).await?;
    Ok(Json(pairs.into_iter().map(|(t, _)| t.into()).collect()))
}

async fn popular_tags_handler(
    State(manager): State<Arc<MediaManager>>,
    Query(q): Query<PopularQuery>,
) -> Result<Json<Vec<PopularTagEntry>>, MediaError> {
    let pairs = manager.popular_tags(q.limit).await?;
    let resp = pairs
        .into_iter()
        .map(|(t, count)| PopularTagEntry {
            tag: t.into(),
            use_count: count,
        })
        .collect();
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
struct PopularTagEntry {
    #[serde(flatten)]
    tag: TagResponse,
    use_count: i64,
}

async fn media_with_tag_handler(
    State(manager): State<Arc<MediaManager>>,
    Path(slug): Path<String>,
    Query(q): Query<ListWithTagQuery>,
) -> Result<Json<Vec<MediaResponse>>, MediaError> {
    let rows = manager.list_with_tag(&slug, q.limit, q.offset).await?;
    // One tag query for the whole page, same as the contents listing.
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|m| match m.id {
            crate::sql::Auto::Set(v) => Some(v),
            _ => None,
        })
        .collect();
    let mut tags = manager.tags_for_many(&ids).await?;
    let mut out = Vec::with_capacity(rows.len());
    for m in rows {
        let id = match m.id {
            crate::sql::Auto::Set(v) => v,
            _ => 0,
        };
        let row_tags = tags.remove(&id).unwrap_or_default();
        out.push(MediaResponse::from_row_with_tags(&manager, m, row_tags).await);
    }
    Ok(Json(out))
}

// Keeps the `MediaStatus` import used even if no handler names it.
#[allow(dead_code)]
fn _media_status_marker(_: MediaStatus) {}
