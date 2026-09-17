//! Axum REST router for the [`MediaManager`] surface.
//!
//! Requires the **`admin`** feature as well as `media`.
//!
//! # This router requires an authorization policy
//!
//! Build it with [`media_router_with`] and supply a
//! [`MediaAuthorizer`]. [`media_router`] is deprecated and refuses
//! every request — it does not serve.
//!
//! That is a behaviour change in 0.57.7, and the reason is worth
//! stating plainly: before it, these 16 routes took **no**
//! authentication, authorization or tenant extractor at all. An
//! anonymous caller could walk the integer id space collecting
//! presigned S3 download links, delete rows by id, and mint a
//! presigned PUT for a key prefix of their choosing. An integrator who
//! copied the quick start below shipped an open bucket.
//!
//! A blanket `.layer(auth)` in front is **not** sufficient for a
//! multi-tenant deployment: no handler carries a tenant, so an
//! authenticated tenant-A user still reads tenant B's row by id.
//! [`MediaTarget`] names the row so the decision can be made per row.
//!
//! Mounted under any prefix you like; the conventional choice is
//! `/media`. All responses are JSON (no auto-CSRF — you wire that
//! at the outer router level via [`crate::forms::csrf`]).
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
//! | GET    | `/collections/{id}/contents`      | Media in the collection. `?recursive=true` to include sub-folders. Authorized as `Read(CollectionContents)` — media rows, not a collection read. |
//! | DELETE | `/collections/{id}`               | Soft-delete a collection **and its descendants** (Media inside orphaned, NOT deleted). Authorized as `Delete(CollectionSubtree)`, not `Delete(Collection)`. |
//! | POST   | `/tags`                           | Create / upsert: body `{slug}`. Authorized as `Add(NewTag)` — a different decision from creating a collection. |
//! | GET    | `/tags`                           | All tags. |
//! | GET    | `/tags/popular`                   | Top tags by usage count. `?limit=N`. |
//! | GET    | `/tags/{slug}/media`              | Media carrying the tag. `?limit=N&offset=N`. |
//!
//! ## Quick start
//!
//! Implementing [`MediaAuthorizer`] needs the `async-trait` attribute;
//! the crate re-exports it as [`crate::media::async_trait`] so you do
//! not add the dependency yourself and the versions cannot drift.
//!
//! ```ignore
//! use rustango::media::{MediaManager, router::media_router_with};
//! use rustango::storage::StorageRegistry;
//!
//! // Tables come from the framework's system migrations (run during
//! // provisioning / `migrate_framework`) — no per-boot bootstrap needed.
//!
//! // `new_pool` takes `sql::Pool` and works on all three backends;
//! // `MediaManager::new` is Postgres-only and takes a `PgPool`.
//! let manager = MediaManager::new_pool(pool.clone(), registry);
//! let app = axum::Router::new()
//!     .nest("/media", media_router_with(manager, MyAuthorizer));
//! ```
//!
//! `MyAuthorizer` is yours — see [`MediaAuthorizer`]. With the
//! **`tenancy`** feature on there is a shipped one, so the secure path
//! is a one-liner:
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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    Media, MediaCollection, MediaError, MediaManager, MediaStatus, MediaTag, UploadIntent,
};

#[allow(dead_code)]
const DEFAULT_PRESIGN_TTL_SECS: u64 = 3600;

/// What a request names — the row an authorizer is deciding about.
///
/// The kind is part of the value on purpose. A bare `Option<i64>` is
/// ambiguous across this route table: `/media/7` and `/collections/7`
/// are different rows in different tables, and an authorizer handed a
/// naked `7` cannot tell them apart. The first version of this API
/// made exactly that mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaTarget {
    /// One media row, by id.
    Media(i64),
    /// One collection row, by id — `GET /collections/{id}`.
    Collection(i64),
    /// The **media inside** one collection — `GET
    /// /collections/{id}/contents` without `?recursive`.
    ///
    /// Separate from [`Self::Collection`] because the two routes return
    /// different tables. This one answers `Vec<MediaResponse>`: media
    /// rows, each carrying a presigned GET URL. Classifying it as a
    /// collection read meant a policy granting "may browse folders" —
    /// `rustango_media_collections.view` under [`MediaPerms`] — read
    /// every media row in the library one collection at a time, and
    /// harvested a signed download link for each.
    ///
    /// The id still names a collection, so a policy that scopes
    /// collections by owner can use it exactly as it uses
    /// [`Self::Collection`]. What changed is that granting it is also a
    /// decision about media.
    CollectionContents(i64),
    /// A collection **and everything under it** — what
    /// `DELETE /collections/{id}` actually reaches.
    ///
    /// Separate from [`Self::Collection`] because the two are different
    /// decisions and the difference is not visible from the request.
    /// Deleting a collection soft-deletes every descendant collection
    /// and sets `collection_id = NULL` on the media in all of them, so
    /// one authorized id can destroy a subtree of any size and re-parent
    /// rows a per-row policy would have refused individually.
    ///
    /// The shape of that subtree is not under the deleting caller's
    /// control: `POST /collections` takes `parent_id` in the body, so
    /// anyone who may create a collection can attach one under someone
    /// else's. The realistic case is not an attacker — it is a shared
    /// library where a colleague parented their folder under yours, and
    /// deleting yours takes theirs with it.
    ///
    /// **Granting this is not the same as granting `Delete(Collection)`.**
    /// A policy that ends on `_ => false`, as the example below does,
    /// denies it until someone opts in deliberately.
    CollectionSubtree(i64),
    /// One tag, by slug. A slug is caller-chosen text — including
    /// text that looks like a number — so it is never an id.
    Tag(String),
    /// `POST /uploads/begin`. Names no existing row: it mints a
    /// presigned `PUT` for a caller-chosen disk and key prefix.
    ///
    /// **Grant this narrowly.** It is a write primitive into storage,
    /// and the caller picks the disk and key prefix. `/uploads/{id}/
    /// finalize` is *not* this — it mutates an existing row and
    /// classifies as `Change(Media(id))`.
    ///
    /// The fields are what the caller **asked for**, read out of the
    /// request body before the handler runs — so a policy can allow-list
    /// a disk, pin a key prefix per tenant, or refuse an upload
    /// attributed to someone else. Without them `rustango_media.add`
    /// means "write anywhere in any bucket", which is not a decision
    /// anyone intended to grant.
    ///
    /// They are **unvalidated caller input**, not facts. A body that
    /// does not parse arrives as empty strings and `None` rather than a
    /// `400`, because authorization is decided before validation is —
    /// the handler rejects a malformed body afterwards, on its own
    /// terms.
    ///
    /// Match it as `MediaTarget::NewUpload { disk, .. }`, with the
    /// trailing `..`. It stays `#[non_exhaustive]` so more of the body
    /// can be surfaced here without breaking a policy written today.
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
    /// optionally under a `parent_id` taken from the body.
    ///
    /// That `parent_id` is why this is worth its own decision.
    /// Collections nest, `DELETE /collections/{id}` takes a whole
    /// subtree, and anyone who may create a collection may attach one
    /// under someone else's — see [`Self::CollectionSubtree`].
    ///
    /// Match as `MediaTarget::NewCollection { .. }`, with the braces —
    /// an empty struct variant so the requested `parent_id` and `slug`
    /// can be added here later without breaking policies written today.
    #[non_exhaustive]
    NewCollection {},
    /// `POST /tags`. Creates (or upserts) a tag by slug.
    ///
    /// Separate from [`Self::NewCollection`] because the two are
    /// different tables and different decisions. They used to arrive as
    /// the same `Add(Listing)`, so a policy could not tell "may create
    /// a folder" from "may create a tag" — the same "one value, two
    /// tables" confusion that `Media(7)` and `Collection(7)` were split
    /// to remove.
    ///
    /// Match as `MediaTarget::NewTag { .. }`, with the braces.
    #[non_exhaustive]
    NewTag {},
    /// A read that enumerates rather than naming a row — `GET
    /// /collections`, `GET /tags`, `GET /tags/popular`, and a
    /// `?recursive` collection listing.
    ///
    /// **Granting this is not harmless.** Listings enumerate, and
    /// enumeration is what turns "guess an id" into "read the index".
    ///
    /// It no longer covers creates: `POST /collections` and `POST
    /// /tags` are [`Self::NewCollection`] and [`Self::NewTag`].
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

/// What a [`MediaAuthorizer`] decided, and therefore what the client is
/// told.
///
/// Three-valued rather than `bool` because **"nobody is signed in" and
/// "signed in, and may not do this" are different answers**, and a
/// client acts on them differently: a token client treats `401` as its
/// cue to refresh, so answering `403` to an anonymous request means the
/// refresh never fires and the member is silently logged out. #1193
/// settled this for `ViewSet`s; media answered `403` to everyone until
/// this existed.
///
/// `From<bool>` is implemented, so a policy that already computes a
/// boolean can `return allowed.into()` — `false` becomes
/// [`Self::Forbidden`], which is the conservative reading of a bare
/// `false` and matches the old behaviour exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaDecision {
    /// Authorized — run the handler.
    Allow,
    /// No authenticated principal. The client is told `401` so it knows
    /// to authenticate.
    ///
    /// Return this only when there is genuinely no identity. A signed-in
    /// user who lacks the permission is [`Self::Forbidden`]: answering
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
    /// Never `Unauthenticated`: a bare `false` carries no information
    /// about whether a principal existed, and guessing `401` from it
    /// would invite the refresh loop the variant exists to avoid.
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
/// **There is no default implementation, deliberately.** Before this
/// existed, `media_router` mounted 16 routes and not one handler took
/// an authentication, authorization or tenant extractor: an anonymous
/// caller could walk the integer id space harvesting presigned S3
/// download links, delete by id, and mint a presigned PUT for a
/// caller-chosen key prefix. An integrator who copied the quick start
/// shipped an open bucket.
///
/// Implement this against whatever your app uses for identity, and
/// scope by tenant here if you are multi-tenant — [`MediaTarget`]
/// names the row for exactly that.
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
/// reaches your policy as a variant you have not written an arm for.
/// Ending on `false` means that arrives denied rather than allowed —
/// and `false.into()` is [`MediaDecision::Forbidden`], never
/// `Unauthenticated`.
///
/// # Your impl runs on every request, and nothing bounds it
///
/// Two measured consequences worth designing around:
///
/// - **There is no timeout here.** A policy that hangs pins its request
///   indefinitely. The gate itself touches no pool, so a wedged request
///   holds no database connection — but a policy that queries holds one
///   from its own pool for as long as it hangs. Mount
///   [`crate::request_timeout`] on the outer router if you want a
///   bound; it covers this layer.
/// - **Do not hold a lock across the `.await`.** "Check a cache, else
///   hit the database" is the natural shape and it compiles, but a
///   `tokio::sync::Mutex` held across the await serializes the whole
///   media surface: eight concurrent requests taking 50 ms each under
///   one lock measured 417 ms rather than ~50 ms. Take the lock, read,
///   drop it, then await.
#[async_trait::async_trait]
pub trait MediaAuthorizer: Send + Sync + 'static {
    /// [`MediaDecision::Allow`] to run the handler;
    /// [`MediaDecision::Unauthenticated`] for `401`;
    /// [`MediaDecision::Forbidden`] for `403`.
    ///
    /// A policy that already computes a boolean can return
    /// `allowed.into()` — `false` is `Forbidden`, which is exactly what
    /// this returned before it was three-valued.
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
        // `Forbidden`, not `Unauthenticated`: the deprecated constructor
        // refuses regardless of who is asking, so telling a client to
        // authenticate would be a lie it could retry forever.
        MediaDecision::Forbidden
    }
}

/// The permission codenames a [`MediaPerms`] request must satisfy —
/// **all** of them, not any.
///
/// `None` means "this variant has no mapping", which
/// [`MediaPerms`] treats as a refusal. That is the case a future
/// `MediaTarget` lands in, and denying it is the point: a new route
/// must not inherit a grant written before it existed.
///
/// Kept as a free function, and public, so the mapping is readable and
/// testable without standing a router up.
#[cfg(feature = "tenancy")]
#[must_use]
pub fn required_codenames(action: &MediaAction) -> Option<&'static [&'static str]> {
    use MediaAction as A;
    use MediaTarget as T;
    Some(match action {
        // `GET /tags/{slug}/media` returns **media** rows, so it is a
        // media read. The tag is the filter, not the subject.
        //
        // `Read(Collection(id))` is two routes, and they are not the
        // same decision: `GET /collections/{id}` returns the collection
        // row, while `GET /collections/{id}/contents` returns
        // `Vec<MediaResponse>` — media rows, each carrying a presigned
        // GET URL. Both used to take the collection codename alone, so
        // "may browse folders" read the library. They are separate
        // targets now; see [`MediaTarget::CollectionContents`].
        A::Read(T::Media(_) | T::Tag(_)) => &["rustango_media.view"],
        A::Read(T::Collection(_)) => &["rustango_media_collections.view"],
        // The listing is of media, so it takes the media permission —
        // and the collection permission too, because the id names a
        // collection the caller must be allowed to open at all.
        A::Read(T::CollectionContents(_)) => {
            &["rustango_media_collections.view", "rustango_media.view"]
        }
        // `Listing` spans three tables — `GET /collections`, `GET
        // /tags`, `GET /tags/popular` and a `?recursive` contents
        // listing. The recursive one is the widest of the four, so the
        // group takes the media permission: every listing here exists
        // to browse the library, and requiring the widest is the
        // fail-closed reading of a target that cannot say which.
        //
        // Kept separate from the `Media | Tag` arm above even though the
        // answer coincides: these are two different reasons for the same
        // codename, and merging them would lose the one that needs
        // stating.
        #[allow(clippy::match_same_arms)]
        A::Read(T::Listing) => &["rustango_media.view"],
        A::Add(T::NewUpload { .. }) => &["rustango_media.add"],
        A::Add(T::NewCollection { .. }) => &["rustango_media_collections.add"],
        A::Add(T::NewTag { .. }) => &["rustango_media_tags.add"],
        A::Change(T::Media(_)) => &["rustango_media.change"],
        A::Delete(T::Media(_)) => &["rustango_media.delete"],
        // Two, and both are needed. Deleting a collection soft-deletes
        // every descendant *and* sets `collection_id = NULL` on the
        // media in all of them — so it writes to `rustango_media`, and
        // a caller who may not change media rows may not do it by
        // deleting the folder they sit in. #1558 is this same point at
        // the target level; this is its codename.
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
/// This exists so the **secure** path is the one-liner. `media_router`
/// refuses every request, and the fastest way back to green from those
/// 403s is an `AllowAll` trait impl — which is the original hole with
/// extra steps (#1546).
///
/// ```ignore
/// use rustango::media::router::{media_router_with, MediaPerms};
///
/// let app = axum::Router::new()
///     .nest("/media", media_router_with(manager, MediaPerms::new(pool)));
/// ```
///
/// Mount it **inside** [`crate::tenancy::middleware::RouterAuthExt::require_auth`]
/// (or `optional_auth`), which is what injects the `AuthenticatedUser`
/// this reads. Without that extension every request is
/// [`MediaDecision::Unauthenticated`] — a `401`, so the symptom names
/// its own cause.
///
/// # What it checks
///
/// [`required_codenames`] has the full mapping. Superusers
/// short-circuit to allow, matching every other gate in this codebase.
///
/// # What it cannot check
///
/// **Codenames are table-level, so this is not row-level.** It cannot
/// express "is this media row yours" or "is this collection in your
/// tenant": a grant of `rustango_media.view` is a grant to read *any*
/// media row by id. `MediaManager` holds a single [`crate::sql::Pool`],
/// so a multi-tenant deployment still has to scope rows itself — that
/// is what [`MediaAuthorizer`] is for, and this type is the floor, not
/// the ceiling.
///
/// The codenames are seeded by `auto_create_permissions`, which runs
/// during tenant provisioning and on migrate. An app upgrading into
/// this can re-seed without a migrate cycle via the `seed-permissions`
/// manage command — the catalog's `UNIQUE (content_type_id, codename)`
/// makes it a no-op on a populated one.
///
/// # Cost
///
/// One indexed permission lookup per required codename, per request,
/// on the pool handed to [`Self::new`]. A superuser pays none. The
/// subtree delete is the only action needing two.
#[cfg(feature = "tenancy")]
pub struct MediaPerms {
    pool: crate::sql::Pool,
    allowed_disks: Option<Vec<String>>,
}

#[cfg(feature = "tenancy")]
impl MediaPerms {
    /// Check permissions against `pool`.
    ///
    /// Under tenancy this should be the tenant's pool, not the
    /// registry's — it is where `rustango_user_permissions` and
    /// `rustango_user_roles` are read from, so the wrong pool means the
    /// wrong tenant's grants.
    ///
    /// Every disk in the [`crate::storage::StorageRegistry`] is
    /// writable by anyone holding `rustango_media.add` until you call
    /// [`Self::allow_disks`]. See that method — on a multi-tenant
    /// deployment it is the difference between "may upload" and "may
    /// upload into any tenant's bucket".
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
    /// on a multi-tenant deployment a bare `rustango_media.add` grant
    /// mints a presigned `PUT` into *any* registered bucket, which is
    /// what [`MediaTarget::NewUpload`]'s own docs call "write anywhere
    /// in any bucket".
    ///
    /// A codename cannot express "this disk" — that is why this is a
    /// list here rather than a permission. It is checked **in addition
    /// to** `rustango_media.add`, never instead of it.
    ///
    /// ```ignore
    /// MediaPerms::new(pool).allow_disks(["user-uploads"])
    /// ```
    ///
    /// Leave it unset only where one disk serves everyone. Prefixes
    /// within a disk are still not expressible here — for
    /// "your own prefix on a shared bucket", implement
    /// [`MediaAuthorizer`], which is handed `key_prefix` for exactly
    /// that.
    #[must_use]
    pub fn allow_disks<I, S>(mut self, disks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_disks = Some(disks.into_iter().map(Into::into).collect());
        self
    }

    /// `true` when `disk` may be written to. Unset means every disk,
    /// which is the documented default and the reason
    /// [`Self::allow_disks`] exists.
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
            // rather than treating the refusal as final (#1193). This is
            // also what a router mounted outside `require_auth` looks
            // like, and 401 names that mistake better than 403 would.
            return MediaDecision::Unauthenticated;
        };
        if auth.is_superuser {
            return MediaDecision::Allow;
        }
        // A variant with no mapping is a route added after this policy
        // was written. Denying it is the point.
        // `/uploads/begin` mints a presigned PUT for a disk and key
        // prefix the caller chooses, and `StorageRegistry` is
        // process-wide — pool-per-tenant isolates the database, not the
        // object store. A codename cannot express "this disk", so a
        // codename check alone is the grant `MediaTarget::NewUpload`'s
        // own docs call "write anywhere in any bucket". Whatever
        // `allowed_disks` says is applied on top of the codename, never
        // instead of it.
        if let MediaAction::Add(MediaTarget::NewUpload { disk, .. }) = &action {
            if !self.disk_allowed(disk) {
                tracing::debug!(
                    target: "rustango::media::auth",
                    disk = %disk,
                    user_id = auth.id,
                    "MediaPerms: upload ticket refused — disk not in the allow-list"
                );
                return MediaDecision::Forbidden;
            }
        }
        let Some(codenames) = required_codenames(&action) else {
            tracing::debug!(
                target: "rustango::media::auth",
                ?action,
                "MediaPerms has no codename for this action — refusing"
            );
            return MediaDecision::Forbidden;
        };
        // An empty slice would fall straight through the loop to
        // `Allow`. Nothing in `required_codenames` returns one today,
        // and this is what keeps that true tomorrow: a mapping that
        // requires nothing is a mistake, not a grant.
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
                    // Fail closed. A driver error is not a grant, and
                    // treating it as one would turn a database blip into
                    // an open bucket.
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
/// the whole struct's default. That is deliberate: the gate must not
/// answer `400`. Authorization is decided first, on whatever the caller
/// sent; the handler validates afterwards, on its own terms. A gate
/// that rejected malformed JSON would be deciding validity before
/// anyone had been asked whether the caller may be here at all.
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
/// An upload-ticket body is a handful of short JSON fields — a few
/// hundred bytes. 16 KiB is far above any real one and far below
/// anything worth calling a denial-of-service primitive, which is what
/// buffering an unbounded body inside an authorization layer would be.
/// Only [`wants_upload_body`] routes are read at all; everything else
/// streams through untouched.
const MAX_GATE_BODY_BYTES: usize = 16 * 1024;

/// Does this request's body have to be read before it can be
/// classified?
///
/// True for `POST /uploads/begin` alone. That route mints a presigned
/// `PUT` for a **caller-chosen** disk and key prefix, and both live in
/// the body — so a gate that never reads a body cannot tell a policy
/// where the write is going.
///
/// Kept as its own function because the middleware has to decide
/// whether to buffer *before* it can call [`classify`], and the route
/// table must not be spelled twice.
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
/// `None` means "this does not match any route in this table", and the
/// caller must refuse. Fail-closed is the whole point: an unrecognised
/// shape that fell through to a permissive default is how a gate gets
/// walked around.
///
/// Segments are matched **positionally** against the route table and
/// percent-decoded first. Both matter, and the first version of this
/// function got both wrong — it scanned for the first integer-parsable
/// segment in the raw path, so `/media/%31` yielded no id at all while
/// the handler served row 1, and `/tags/2024/media` yielded `2024` as
/// if a caller-chosen tag slug were an object id.
///
/// `query` is read for one thing only: `?recursive` on a collection's
/// contents reaches media in descendant collections, so the request
/// touches more rows than the one its path names. That widening is
/// classified as [`MediaTarget::Listing`], whose docs say plainly that
/// granting it is not harmless.
///
/// `upload` carries the parsed body for the one route that needs it —
/// see [`wants_upload_body`]. It is `None` for every other route, and a
/// `None` on `POST /uploads/begin` yields a target with empty fields
/// rather than no target: a body the gate could not read is still an
/// upload request, and the policy is what should refuse it.
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

    // Presence, not value: the handler rejects a non-boolean, and a
    // caller who names the parameter at all is asking to widen.
    //
    // The **key is decoded first**, with form semantics, because that is
    // what the handler's `Query` extractor does — `serde_urlencoded`
    // goes through `form_urlencoded::parse`, which percent-decodes the
    // key and turns `+` into a space. Matching the raw bytes here let
    // `?%72ecursive=true` reach the handler as a subtree walk while the
    // gate classified it as a read of the one collection named, which is
    // the same disagreement `percent_decode_path` was introduced to
    // close on the path segments. The query side was left raw.
    //
    // Presence still wins over value, so a bare `?recursive` is treated
    // as widening even though the handler answers 400 to it. The gate
    // being *stricter* than the handler is the safe direction, and
    // loosening this to match the handler exactly would reopen the gap
    // from the other side.
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
        // Finalize mutates the media row it names — `finalize_upload`
        // loads `rustango_media` by this id — so it is a `Media`
        // target, not a kind of its own. An `Upload(i64)` variant here
        // said "different row" about the same row.
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
        // A recursive listing reaches descendants the authorizer is
        // never asked about, so it is not a single-collection read.
        (&Method::GET, ["collections", _, "contents"]) if recursive => {
            Some(MediaAction::Read(MediaTarget::Listing))
        }
        // The collection row itself.
        (&Method::GET, ["collections", raw]) => {
            Some(MediaAction::Read(MediaTarget::Collection(id(raw)?)))
        }
        // The media inside it — a different table, and a presigned URL
        // per row. Sharing `Collection` with the arm above is what let
        // `rustango_media_collections.view` read the library.
        (&Method::GET, ["collections", raw, "contents"]) => {
            Some(MediaAction::Read(MediaTarget::CollectionContents(id(raw)?)))
        }
        // Not `Collection` — this route deletes the whole subtree and
        // orphans the media under every level of it. Naming one id in a
        // target that means "one row" understated it by however many
        // descendants the tree happens to hold.
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
                    // No classification means no route in this table matches
                    // (or an id that cannot be a row id). Refuse: passing an
                    // unrecognised shape through is how a gate is walked
                    // around.
                    //
                    // Mounted at the root this also covers the fallback, so
                    // a probe gets 403 instead of a 404 that maps the
                    // surface. Mounted with `nest` — which is what the
                    // quick start recommends — it does not: `nest` keeps
                    // the outer router's fallback, so an unmatched path
                    // under the prefix 404s before reaching here. Nothing
                    // is served either way; the difference is only how much
                    // an unauthenticated prober can infer.
                    //
                    // One route needs its body to be classified, and it
                    // is the one that mints a presigned PUT into
                    // storage. Buffered under a cap and handed straight
                    // back to the handler, so nothing downstream can
                    // tell — except that the policy now knows which
                    // disk and prefix it is being asked to allow.
                    let (upload, body) = if wants_upload_body(&parts.method, parts.uri.path()) {
                        // Over the cap, or the stream failed. Nothing
                        // legitimate sends 16 KiB of upload-ticket JSON,
                        // and refusing here is what keeps the gate from
                        // being a place to make a server buffer.
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
                        // request. It arrives at the policy with empty
                        // fields rather than as a 400 — the gate decides
                        // authorization, the handler decides validity,
                        // in that order.
                        let detail = serde_json::from_slice::<UploadRequestDetail>(&bytes)
                            .unwrap_or_default();
                        (Some(detail), axum::body::Body::from(bytes))
                    } else {
                        (None, body)
                    };
                    let Some(action) =
                        classify(&parts.method, parts.uri.path(), parts.uri.query(), upload)
                    else {
                        tracing::debug!(
                            target: "rustango::media::auth",
                            method = %parts.method,
                            path = %parts.uri.path(),
                            "refused: no route in the media table matches"
                        );
                        return (
                            StatusCode::FORBIDDEN,
                            Json(
                                serde_json::json!({ "error": "not a recognised media operation" }),
                            ),
                        )
                            .into_response();
                    };
                    let decision = auth.authorize(&parts, action.clone()).await;
                    if decision != MediaDecision::Allow {
                        // `debug`, so it costs nothing in production. Without
                        // it a subtly-wrong policy is only debuggable by
                        // instrumenting inside the integrator's own impl.
                        tracing::debug!(
                            target: "rustango::media::auth",
                            ?action,
                            ?decision,
                            method = %parts.method,
                            path = %parts.uri.path(),
                            "refused by the authorization policy"
                        );
                        // 401 means "authenticate", 403 means "you cannot do
                        // this". A token client treats 401 as its cue to
                        // refresh; answering 403 to an anonymous request
                        // means the refresh never fires (#1193).
                        let (status, message) = match decision {
                            MediaDecision::Unauthenticated => {
                                (StatusCode::UNAUTHORIZED, "authentication required")
                            }
                            _ => (
                                StatusCode::FORBIDDEN,
                                "not authorized for this media operation",
                            ),
                        };
                        return (status, Json(serde_json::json!({ "error": message })))
                            .into_response();
                    }
                    next.run(axum::extract::Request::from_parts(parts, body))
                        .await
                }
            },
        ))
        // Outermost, so it also covers the gate's own early 403s — the
        // last `.layer` wraps the ones before it. Applied inside the
        // gate, a refusal returned before ever reaching this.
        .layer(axum::middleware::from_fn(no_store))
}

/// `Cache-Control: no-store` and `Vary` on every response.
///
/// These routes hand out **presigned URLs** — short-lived bearer
/// credentials, valid for anyone holding them until the TTL expires.
/// Without a directive, a 200 `GET` with no explicit freshness is
/// heuristically cacheable (RFC 9111 §4.2.2), and with no `Vary` the
/// cache key is method plus URI.
///
/// That matters because RFC 9111 §3.5 keeps a shared cache off a
/// response whose request carried `Authorization`, and says **nothing
/// about `Cookie`** — while the authorizer this module documents is
/// cookie/session shaped. A CDN in front of such a deployment could
/// store one user's signed link and serve it to the next caller of the
/// same URI.
///
/// Applied to the whole surface rather than only the routes that embed
/// a signature today, so a route added later cannot quietly opt out.
/// `no-store` rather than `private`: `private` still permits a browser
/// cache to keep the credential on disk.
///
/// # Scope
///
/// This covers the JSON **carrying** a presigned URL. It says nothing
/// about the subsequent fetch of the object through that URL, which is
/// a different request to a different origin under its own cache rules.
///
/// It also cannot reach a response this router never produced. Mounted
/// with `nest` — the quick start's shape — an unmatched path under the
/// prefix is answered by the *outer* router's fallback and carries
/// neither header. Nothing is served there, so no credential leaks; it
/// is an inconsistency rather than a hole.
///
/// Giving this router its own `.fallback()` would close that, and was
/// considered. Measured against axum 0.8: nested it does close it
/// (404 → 403), and a path outside the prefix still 404s correctly.
/// But `Router::merge` does **not** reject two fallbacks — an
/// integrator who merges instead of nesting silently gets 403 for every
/// unknown URL in their whole application. Trading a bodyless 404 for a
/// silent, hard-to-debug hijack of someone else's routing is the wrong
/// side of that deal.
async fn no_store(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    // `insert`, not `append`. A handler that set its own `Cache-Control`
    // would otherwise leave two directives on the response, and the
    // weaker one could win.
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    // `Vary` as well as `Cache-Control`, because the two are not the
    // same kind of instruction. `no-store` *asks* a cache not to store;
    // `Vary` *compels* it, by changing the cache key rather than
    // requesting permission.
    //
    // That distinction is the whole point here. The deployment this
    // guards against — a CDN in front of a cookie-authenticated app —
    // is exactly the population where overriding origin directives is
    // routine: nginx `proxy_ignore_headers Cache-Control`, Cloudflare
    // "Cache Everything", Fastly VCL setting its own TTL. In each of
    // those `no-store` is discarded and the original bug is back.
    // Ignoring `Vary` takes a separate, deliberate second step.
    //
    // The usual objection — that high-cardinality values destroy hit
    // rate — is the intended outcome on this surface, so it inverts
    // into an argument for it. Both header names, because the
    // authorizer reads `Parts` and may key off either.
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
/// That is deliberate and it is a behaviour change: this constructor
/// used to serve the whole media surface to anyone who could reach it.
///
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
        let (status, msg) = match &self {
            MediaError::UnknownDisk(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            MediaError::Storage(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            MediaError::Db(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            MediaError::Other(m) if m.contains("not found") => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            MediaError::Other(_) => (StatusCode::BAD_REQUEST, self.to_string()),
        };
        let body = serde_json::json!({"error": msg});
        (status, Json(body)).into_response()
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
    // One tag query for the whole page rather than one per row. The
    // loop below used to run `tags_for` per row, so a page cost
    // 1 + N round trips.
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
    // No dedicated `list_all_tags` method on the manager — popular()
    // with a high limit covers the same ground, sorted by usage.
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
    let mut out = Vec::with_capacity(rows.len());
    for m in rows {
        out.push(MediaResponse::from_row(&manager, m).await?);
    }
    Ok(Json(out))
}

// Avoid an unused-import warning if MediaStatus isn't referenced
// elsewhere in the module after future trims.
#[allow(dead_code)]
fn _media_status_marker(_: MediaStatus) {}
