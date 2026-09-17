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
//! | POST   | `/collections`                    | Create: body `{name, slug, parent_id?, description?}`. |
//! | GET    | `/collections`                    | List every non-deleted collection. |
//! | GET    | `/collections/{id}`               | Single collection. |
//! | GET    | `/collections/{id}/contents`      | Media in the collection. `?recursive=true` to include sub-folders. |
//! | DELETE | `/collections/{id}`               | Soft-delete a collection **and its descendants** (Media inside orphaned, NOT deleted). |
//! | POST   | `/tags`                           | Create / upsert: body `{slug}`. |
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
//! `MyAuthorizer` is yours — see [`MediaAuthorizer`]. There is no
//! default, because the default that existed before was "allow
//! everyone".

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
    /// One collection, by id.
    Collection(i64),
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
    /// Match it as `MediaTarget::NewUpload { .. }`. It is an empty
    /// struct variant rather than a unit one so that the requested
    /// `disk` and `key_prefix` can be added here later (#1546) without
    /// breaking policies written today — a `#[non_exhaustive]` *unit*
    /// variant cannot be matched outside this crate at all.
    #[non_exhaustive]
    NewUpload {},
    /// A listing or a create — `GET /collections`, `GET /tags`,
    /// `GET /tags/popular`, `POST /collections`, `POST /tags`.
    ///
    /// **Granting this is not harmless.** Listings enumerate, and
    /// enumeration is what turns "guess an id" into "read the index".
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
///     async fn authorize(&self, parts: &Parts, action: MediaAction) -> bool {
///         let Some(user) = current_user(parts) else { return false };
///         match action {
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
///             // prefix the *caller* chooses. Grant it only to accounts
///             // you would trust with the bucket — it is not the same
///             // decision as "may create a collection".
///             MediaAction::Add(MediaTarget::NewUpload { .. }) => user.is_trusted_uploader(),
///             MediaAction::Add(_) => user.is_editor(),
///             MediaAction::Change(MediaTarget::Media(id)) => user.owns_media(id).await,
///             MediaAction::Delete(MediaTarget::Media(id)) => user.owns_media(id).await,
///             _ => false,
///         }
///     }
/// }
/// ```
///
/// Note the trailing `_ => false` arms. [`MediaAction`] and
/// [`MediaTarget`] are both `#[non_exhaustive]`, so a future route
/// reaches your policy as a variant you have not written an arm for.
/// Ending on `false` means that arrives denied rather than allowed.
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
    /// `true` to allow. Anything else is a 403.
    async fn authorize(&self, parts: &axum::http::request::Parts, action: MediaAction) -> bool;
}

/// So a host that picks its policy at runtime can pass
/// `Arc<dyn MediaAuthorizer>`, which the bare generic bound rejects.
#[async_trait::async_trait]
impl<T: MediaAuthorizer + ?Sized> MediaAuthorizer for Arc<T> {
    async fn authorize(&self, parts: &axum::http::request::Parts, action: MediaAction) -> bool {
        (**self).authorize(parts, action).await
    }
}

/// Refuses everything. What [`media_router`] uses.
struct DenyAll;

#[async_trait::async_trait]
impl MediaAuthorizer for DenyAll {
    async fn authorize(&self, _: &axum::http::request::Parts, _: MediaAction) -> bool {
        false
    }
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
fn classify(method: &axum::http::Method, path: &str, query: Option<&str>) -> Option<MediaAction> {
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
    let recursive = query.is_some_and(|q| {
        q.split('&')
            .any(|p| p == "recursive" || p.starts_with("recursive="))
    });

    match (m, s.as_slice()) {
        (&Method::POST, ["uploads", "begin"]) => Some(MediaAction::Add(MediaTarget::NewUpload {})),
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
        (&Method::POST, ["collections"]) => Some(MediaAction::Add(MediaTarget::Listing)),
        // A recursive listing reaches descendants the authorizer is
        // never asked about, so it is not a single-collection read.
        (&Method::GET, ["collections", _, "contents"]) if recursive => {
            Some(MediaAction::Read(MediaTarget::Listing))
        }
        (&Method::GET, ["collections", raw] | ["collections", raw, "contents"]) => {
            Some(MediaAction::Read(MediaTarget::Collection(id(raw)?)))
        }
        (&Method::DELETE, ["collections", raw]) => {
            Some(MediaAction::Delete(MediaTarget::Collection(id(raw)?)))
        }

        // `popular` is a listing, and must be matched before the
        // `{slug}` arm or a tag literally named "popular" shadows it.
        (&Method::GET, ["tags"] | ["tags", "popular"]) => {
            Some(MediaAction::Read(MediaTarget::Listing))
        }
        (&Method::POST, ["tags"]) => Some(MediaAction::Add(MediaTarget::Listing)),
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
                    let Some(action) = classify(&parts.method, parts.uri.path(), parts.uri.query())
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
                    if !auth.authorize(&parts, action.clone()).await {
                        // `debug`, so it costs nothing in production. Without
                        // it a subtly-wrong policy is only debuggable by
                        // instrumenting inside the integrator's own impl.
                        tracing::debug!(
                            target: "rustango::media::auth",
                            ?action,
                            method = %parts.method,
                            path = %parts.uri.path(),
                            "refused by the authorization policy"
                        );
                        return (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({
                                "error": "not authorized for this media operation"
                            })),
                        )
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

/// `Cache-Control: no-store` on every response from this router.
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
async fn no_store(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
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
