//! **rustango** — a Django-shaped, batteries-included web framework for Rust.
//!
//! It is built on one derive macro and one connection type, and ships a typed
//! ORM with auto-migrations, an auto-generated admin, multi-tenancy, auth
//! (sessions / JWT / OAuth2-OIDC / HMAC / passkeys), DRF-style serializers and
//! viewsets, signals, caching, media, email, background jobs, scheduled tasks,
//! OpenAPI 3.1, and the usual production middleware.
//!
//! **Postgres, MySQL and SQLite** all use the same [`sql::Pool`] and the same
//! `#[derive(Model)]` types. The backend is a Cargo feature, not a rewrite.
//!
//! Everything is opt-in: take only the features you use, down to the bare ORM
//! with no HTTP stack. Release history is in the
//! [CHANGELOG](https://github.com/ujeenet/rustango/blob/main/CHANGELOG.md).
//!
//! # Install
//!
//! ```toml
//! [dependencies]
//! rustango = "0.57"                                        # Postgres (the default backend)
//! # or pick another backend — see "Choosing a backend" below:
//! rustango = { version = "0.57", default-features = false, features = ["sqlite", "batteries"] }
//! rustango = { version = "0.57", default-features = false, features = ["mysql",  "batteries"] }
//! ```
//!
//! `default = ["postgres", "batteries"]`. **`batteries`** is everything except
//! the backend (admin, serializers, auth, jobs, cache, media, middleware, …).
//! You pick the backend separately, so you can keep the batteries and still
//! choose your database. Drop `batteries` for the bare ORM (no axum, no Tera).
//!
//! # A taste
//!
//! ```ignore
//! use rustango::Model;
//! use rustango::sql::{Auto, Pool, FetcherPool};
//!
//! // One derive gives a table, typed columns, migrations, and admin wiring.
//! #[derive(Model, Clone, Debug)]
//! #[rustango(table = "posts")]
//! struct Post {
//!     #[rustango(primary_key)]
//!     id: Auto<i64>,                 // server-assigned auto-increment PK
//!     #[rustango(max_length = 200)]
//!     title: String,
//!     body: String,
//!     published: bool,
//! }
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! // One connection type for every backend — the driver is chosen from the
//! // URL scheme (postgres://…, mysql://…, sqlite://…).
//! let pool = Pool::connect(&std::env::var("DATABASE_URL")?).await?;
//!
//! // Django-shape queries, compiled to your dialect.
//! let recent: Vec<Post> = Post::objects()
//!     .filter("published", true)
//!     .order_by_desc("id")
//!     .limit(10)
//!     .fetch(&pool)
//!     .await?;
//! # let _ = recent; Ok(())
//! # }
//! ```
//!
//! # Choosing a backend
//!
//! The same models and queries emit Postgres, MySQL or SQLite SQL. You pick
//! the backend with a Cargo feature and the `DATABASE_URL` scheme.
//!
//! - **Connect with [`sql::Pool::connect`].** It reads the URL scheme and
//!   builds the right pool. Prefer it over `sqlx::PgPool::connect(...)`, which
//!   only exists when the `postgres` feature is on.
//! - **[`sql::Pool`]** is the enum every rustango API takes (`&Pool`). A
//!   concrete `sqlx::PgPool` / `SqlitePool` / `MySqlPool` converts into it
//!   with `From`, so you can pass either.
//! - **Enable one backend feature** (`postgres`, `mysql`, or `sqlite`) for a
//!   single-database app. Enable several to build one binary that serves any
//!   of them.
//! - Multi-tenancy works on all three: [`tenancy::TenantPools`] is generic
//!   over the backend. Schema-mode tenancy is Postgres-only, because it uses
//!   `SET search_path`.
//!
//! # Feature flags
//!
//! | Feature | Gives you |
//! |---|---|
//! | `postgres` / `mysql` / `sqlite` | The database backend(s). |
//! | `batteries` | The default bundle minus the backend (see [Install](#install)). |
//! | `admin` | Auto-generated admin UI + session auth. |
//! | `tenancy` | Multi-tenant resolver, per-tenant pools, operator console. |
//! | `serializer` | DRF-style serializers + `#[derive(ViewSet)]` REST endpoints. |
//! | `jwt` / `oauth2` | Token auth; social / OIDC login. |
//! | `jobs` / `jobs-postgres` | Background jobs (in-memory / durable). |
//! | `cache` / `cache-redis` | Cache layer; Redis backend. |
//! | `storage` / `storage-s3` / `media` | File storage + the `Media` model. |
//! | `mcp` | Model Context Protocol server over HTTP. |
//! | `openapi` | OpenAPI 3.1 schema auto-derive. |
//!
//! Every feature is listed in the crate's `Cargo.toml` with a one-line note.
//!
//! # Quick start (scaffolder)
//!
//! ```bash
//! cargo install cargo-rustango
//! cargo rustango new myblog                 # default: ORM + admin
//! cargo rustango new myapi --template api   # JSON-only, no admin
//! cargo rustango new shop --template tenant # multi-tenancy + operator console
//!
//! cd myblog
//! cp .env.example .env                      # set DATABASE_URL
//! cargo run -- migrate                      # apply bootstrap migrations
//! cargo run                                 # http://localhost:8080
//! ```
//!
//! # The manage runner
//!
//! One binary serves HTTP and dispatches CLI commands: `cargo run` starts the
//! server, `cargo run -- <verb>` runs a command. A scaffolded `main.rs`:
//!
//! ```ignore
//! #[rustango::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let _ = dotenvy::dotenv();
//!     rustango::manage::Cli::new()
//!         .api(myblog::urls::api())
//!         .tenancy()                         // optional, with the `tenancy` feature
//!         .run()
//!         .await
//! }
//! ```
//!
//! Common verbs (each supports `--help`):
//!
//! | Group | Verbs |
//! |---|---|
//! | Migrations | `makemigrations`, `migrate [target]`, `downgrade [N]`, `showmigrations` |
//! | Scaffolders | `startapp`, `make:viewset`, `make:serializer`, `make:form`, `make:job`, `make:middleware`, `make:test` |
//! | System | `about`, `check`, `check --deploy`, `version` |
//! | Tenancy | `create-tenant`, `create-operator`, `create-user`, `list-tenants`, `create-api-key`, `grant-perm`, `audit-cleanup` |
//!
//! # Where to look
//!
//! - Models & queries — [`core`], [`sql`]
//! - Admin — [`admin`]
//! - REST — [`viewset`], [`serializer`]
//! - Multi-tenancy — [`tenancy`]
//! - Auth — [`auth_backends`], [`jwt`], [`sso`]
//! - Background work — [`jobs`], [`scheduler`]
//! - Caching — [`cache`]
//!
//! The workspace [README](https://github.com/ujeenet/rustango) has the full
//! feature matrix and guides.
//! [`examples/cookbook_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/cookbook_blog)
//! is a runnable multi-tenant blog with one chapter per feature.

// Lets `::rustango::...` paths emitted by the proc-macro resolve to
// ourselves inside this crate.
extern crate self as rustango;

// ---------------------------------------------------------------- per-backend macro support
//
// `#[derive(Model)]` always emits a call to `__impl_my_from_row!`. The two
// definitions below are gated on rustango's own `mysql` feature: with the
// feature on it expands to a real `sqlx::FromRow<MySqlRow>` impl, with the
// feature off it expands to nothing.
//
// The proc macro cannot see the user's feature set at expansion time, but
// macro_rules expand after it is resolved, so the right arm fires on its own.
// Users need no `mysql = ["rustango/mysql"]` shim feature of their own.

#[doc(hidden)]
#[cfg(feature = "mysql")]
#[macro_export]
macro_rules! __impl_my_from_row {
    // The caller passes its own `row` identifier so the function parameter
    // and the body's `row.try_get(...)` share one hygiene context. Without
    // it the body cannot see `$row` and name resolution fails.
    ($struct:ty, |$row:ident| { $($body:tt)* }) => {
        impl<'r> $crate::sql::sqlx::FromRow<'r, $crate::sql::sqlx::mysql::MySqlRow>
            for $struct
        {
            fn from_row(
                $row: &'r $crate::sql::sqlx::mysql::MySqlRow,
            ) -> ::core::result::Result<Self, $crate::sql::sqlx::Error> {
                ::core::result::Result::Ok(Self { $($body)* })
            }
        }
    };
}

#[doc(hidden)]
#[cfg(not(feature = "mysql"))]
#[macro_export]
macro_rules! __impl_my_from_row {
    ($struct:ty, |$row:ident| { $($body:tt)* }) => {};
}

// ---- select_related's aliased-row decoder for MySQL ----
//
// `Model::__rustango_from_aliased_row(row, prefix)` is the per-model helper
// the executor calls when decoding LEFT-JOINed columns into an FK target.
// This is its MySQL-typed twin, using the same hygiene pattern as above.

#[doc(hidden)]
#[cfg(feature = "mysql")]
#[macro_export]
macro_rules! __impl_my_aliased_row_decoder {
    ($struct:ty, |$row:ident, $prefix:ident| { $($body:tt)* }) => {
        impl $struct {
            #[doc(hidden)]
            pub fn __rustango_from_aliased_my_row(
                $row: &$crate::sql::sqlx::mysql::MySqlRow,
                $prefix: &str,
            ) -> ::core::result::Result<Self, $crate::sql::sqlx::Error> {
                ::core::result::Result::Ok(Self { $($body)* })
            }
        }
    };
}

#[doc(hidden)]
#[cfg(not(feature = "mysql"))]
#[macro_export]
macro_rules! __impl_my_aliased_row_decoder {
    ($struct:ty, |$row:ident, $prefix:ident| { $($body:tt)* }) => {};
}

// ---- LoadRelated dual (PgRow + MySqlRow) impl ----
//
// The MySQL counterpart of the PG `LoadRelated` impl. Same shape, but it
// implements `sql::LoadRelatedMy` over `MySqlRow`.

#[doc(hidden)]
#[cfg(feature = "mysql")]
#[macro_export]
macro_rules! __impl_my_load_related {
    // The arms mutate `self.<field>`, and `self` is hygiene-tracked like any
    // other identifier. So the caller passes `$self` explicitly; dropping it
    // gives "expected value, found module `self`" on the first FK arm.
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident, $rest:ident, $next:ident| {
        $($arms:tt)*
    }) => {
        impl $crate::sql::LoadRelatedMy for $struct {
            #[allow(unused_variables)]
            fn __rustango_load_related_my(
                &mut self,
                $row: &$crate::sql::sqlx::mysql::MySqlRow,
                $field: &str,
                $alias: &str,
            ) -> ::core::result::Result<bool, $crate::sql::sqlx::Error> {
                let $self_ = self;
                // Split the multi-hop path: the arms match the base FK on
                // this model and recurse on `$rest` at the `$next` alias.
                let (__base, $rest): (&str, ::core::option::Option<&str>) =
                    match $field.split_once("__") {
                        ::core::option::Option::Some((b, r)) => (b, ::core::option::Option::Some(r)),
                        ::core::option::Option::None => ($field, ::core::option::Option::None),
                    };
                let $next: ::std::string::String = match $rest {
                    ::core::option::Option::Some(__r) => {
                        let __rb = __r.split_once("__").map(|(b, _)| b).unwrap_or(__r);
                        ::std::format!("{}__{}", $alias, __rb)
                    }
                    ::core::option::Option::None => ::std::string::String::new(),
                };
                match __base {
                    $($arms)*
                    _ => ::core::result::Result::Ok(false),
                }
            }
        }
    };
}

#[doc(hidden)]
#[cfg(not(feature = "mysql"))]
#[macro_export]
macro_rules! __impl_my_load_related {
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident, $rest:ident, $next:ident| {
        $($arms:tt)*
    }) => {};
}

// ---- SQLite counterparts to the three MySQL macros above ----
//
// Same pattern, gated on `sqlite` instead of `mysql`. The proc-macro always
// emits a call to each; these collapse to nothing when the feature is off.

#[doc(hidden)]
#[cfg(feature = "sqlite")]
#[macro_export]
macro_rules! __impl_sqlite_from_row {
    ($struct:ty, |$row:ident| { $($body:tt)* }) => {
        impl<'r> $crate::sql::sqlx::FromRow<'r, $crate::sql::sqlx::sqlite::SqliteRow>
            for $struct
        {
            fn from_row(
                $row: &'r $crate::sql::sqlx::sqlite::SqliteRow,
            ) -> ::core::result::Result<Self, $crate::sql::sqlx::Error> {
                ::core::result::Result::Ok(Self { $($body)* })
            }
        }
    };
}

#[doc(hidden)]
#[cfg(not(feature = "sqlite"))]
#[macro_export]
macro_rules! __impl_sqlite_from_row {
    ($struct:ty, |$row:ident| { $($body:tt)* }) => {};
}

#[doc(hidden)]
#[cfg(feature = "sqlite")]
#[macro_export]
macro_rules! __impl_sqlite_aliased_row_decoder {
    ($struct:ty, |$row:ident, $prefix:ident| { $($body:tt)* }) => {
        impl $struct {
            #[doc(hidden)]
            pub fn __rustango_from_aliased_sqlite_row(
                $row: &$crate::sql::sqlx::sqlite::SqliteRow,
                $prefix: &str,
            ) -> ::core::result::Result<Self, $crate::sql::sqlx::Error> {
                ::core::result::Result::Ok(Self { $($body)* })
            }
        }
    };
}

#[doc(hidden)]
#[cfg(not(feature = "sqlite"))]
#[macro_export]
macro_rules! __impl_sqlite_aliased_row_decoder {
    ($struct:ty, |$row:ident, $prefix:ident| { $($body:tt)* }) => {};
}

#[doc(hidden)]
#[cfg(feature = "sqlite")]
#[macro_export]
macro_rules! __impl_sqlite_load_related {
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident, $rest:ident, $next:ident| {
        $($arms:tt)*
    }) => {
        impl $crate::sql::LoadRelatedSqlite for $struct {
            #[allow(unused_variables)]
            fn __rustango_load_related_sqlite(
                &mut self,
                $row: &$crate::sql::sqlx::sqlite::SqliteRow,
                $field: &str,
                $alias: &str,
            ) -> ::core::result::Result<bool, $crate::sql::sqlx::Error> {
                let $self_ = self;
                // Split the multi-hop path (see the PG twin).
                let (__base, $rest): (&str, ::core::option::Option<&str>) =
                    match $field.split_once("__") {
                        ::core::option::Option::Some((b, r)) => (b, ::core::option::Option::Some(r)),
                        ::core::option::Option::None => ($field, ::core::option::Option::None),
                    };
                let $next: ::std::string::String = match $rest {
                    ::core::option::Option::Some(__r) => {
                        let __rb = __r.split_once("__").map(|(b, _)| b).unwrap_or(__r);
                        ::std::format!("{}__{}", $alias, __rb)
                    }
                    ::core::option::Option::None => ::std::string::String::new(),
                };
                match __base {
                    $($arms)*
                    _ => ::core::result::Result::Ok(false),
                }
            }
        }
    };
}

#[doc(hidden)]
#[cfg(not(feature = "sqlite"))]
#[macro_export]
macro_rules! __impl_sqlite_load_related {
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident, $rest:ident, $next:ident| {
        $($arms:tt)*
    }) => {};
}

pub mod audit;
/// ContentType framework (Django-shape) — a runtime handle for "any
/// registered model". Used by permissions, generic foreign keys, soft-FK
/// prefetch and audit-history admin panels.
pub mod contenttypes;
pub mod core;
/// Named multi-database registry + `QuerySet::using(alias)`.
pub mod databases;
pub mod migrate;
pub mod query;
pub mod sql;

/// Test-support helpers (schema builders from `Model::SCHEMA` + model
/// factories). Dev-only: `#[cfg(test)]` for this crate's own tests,
/// `feature = "testkit"` for external integration tests.
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

/// Soft-delete query helpers — `active_filter` / `trashed_filter` /
/// `compose_with_active` / `soft_delete` / `restore` / `purge` for any
/// model carrying `#[rustango(soft_delete)]`. See [`soft_delete`].
pub mod soft_delete;

#[cfg(feature = "admin")]
pub mod admin;

#[cfg(feature = "config")]
pub mod config;

#[cfg(feature = "forms")]
pub mod forms;

/// DRF-style serializer layer — `#[derive(Serializer)]` + [`serializer::ModelSerializer`].
/// Typed JSON output from model instances with field control and validation.
#[cfg(feature = "serializer")]
pub mod serializer;

/// Pluggable caching layer — [`cache::Cache`] trait + [`cache::NullCache`] +
/// [`cache::InMemoryCache`]. Redis backend behind the `cache-redis` feature.
#[cfg(feature = "cache")]
pub mod cache;

/// Fragment caching — Django's `{% cache %}` template tag.
/// [`cache_fragment::cached_render`] checks the cache before it calls your
/// compute closure. It is a handler-side helper rather than a Tera block
/// tag, because Tera has no block-tag extension API.
#[cfg(feature = "cache")]
pub mod cache_fragment;

/// Per-view caching tower layer + `Cache-Control` / `Vary` header builders.
/// Django's `@cache_page` / `@cache_control` / `@vary_on_*` analogs.
#[cfg(feature = "cache-page")]
pub mod cache_page;

/// Django-shape model signals — [`signals::connect_post_save`] etc.
/// Receivers register globally per model type and run sequentially.
#[cfg(feature = "signals")]
pub mod signals;

/// Domain event bus — typed pub-sub for application events, separate from
/// the ORM. Use it for cross-component fan-out (`OrderPlaced` → mail +
/// billing + audit). For per-model lifecycle hooks use [`signals`] instead.
/// See [`events::EventBus`].
pub mod events;

/// Model pruning — declarative bulk removal of stale rows. See
/// [`prunable::Prunable`] + [`register_prunable!`].
pub mod prunable;

/// CORS middleware — [`cors::CorsLayer`] for axum routers.
#[cfg(feature = "admin")]
pub mod cors;

/// Token-bucket rate limiting middleware — [`rate_limit::RateLimitLayer`].
/// Per-IP, per-header, or global. Returns 429 with `Retry-After` when exhausted.
#[cfg(feature = "admin")]
pub mod rate_limit;

/// Cache-backed rate limiting middleware — a fixed-window counter over the
/// [`cache::Cache`] trait. Pair it with `RedisCache` to enforce one limit
/// across several processes. See [`rate_limit_cache::CacheRateLimitLayer`].
#[cfg(all(feature = "admin", feature = "cache"))]
pub mod rate_limit_cache;

/// Health check endpoints — `/health` (liveness) + `/ready` (readiness).
/// See [`health::health_router`].
#[cfg(feature = "admin")]
pub mod health;

/// Email backends — [`email::Mailer`] trait + console/in-memory/null backends.
#[cfg(feature = "email")]
pub mod email;

/// Tera-rendered email helpers — bridge [`crate::email`] mailers and
/// the Tera templating engine. Each email = `<name>.subject.txt` +
/// `<name>.txt` + optional `<name>.html`. See
/// [`email_templates::EmailRenderer`].
#[cfg(all(feature = "email", feature = "admin"))]
pub mod email_templates;

/// Send email off the request path through the [`crate::jobs`] queue, so a
/// temporary SMTP failure retries instead of failing the handler.
/// See [`email_jobs::register_email_job`] + [`email_jobs::dispatch_email`].
#[cfg(all(feature = "email", feature = "jobs"))]
pub mod email_jobs;

/// Laravel-shape `Mailable` trait — an email type as a struct that owns its
/// template and recipient logic. Pairs with
/// [`email_templates::EmailRenderer`] and [`email_jobs`].
/// See [`mailable::Mailable`].
#[cfg(all(feature = "email", feature = "admin"))]
pub mod mailable;

/// JSON:API v1.1 response envelope — wrap a flat `serde_json::Value`
/// (typical [`serializer`] output) in the `{"data": {"type", "id",
/// "attributes": {...}}}` shape clients expect. See
/// [`jsonapi::to_resource`] + [`jsonapi::to_collection`].
pub mod jsonapi;

/// Shared HMAC-SHA256 / SHA-256 / hex primitives, plus `constant_time_eq`.
/// Internal: for raw HMAC, depend on `hmac` + `sha2` directly. The cfg list
/// covers every feature that needs one of these primitives.
#[cfg(any(
    feature = "hmac-auth",
    feature = "storage-s3",
    feature = "signed_url",
    feature = "jwt",
    feature = "csrf",
    feature = "totp",
))]
pub(crate) mod crypto;

/// Attribute casts — `Cast<C>` field wrapper + `CastValue` trait, with the
/// `EncryptedString` AEAD-at-rest built-in.
#[cfg(feature = "casts")]
pub mod casts;

/// Lowercase-hex codec, no external deps. It sits outside
/// [`crate::crypto`] so always-on callers (pagination cursors,
/// `row_to_json` binary arms) can use it without the crypto deps.
pub(crate) mod hex;

/// URL / IRI / URI helpers — `django.utils.encoding` +
/// `django.utils.http` parity: `url_encode`, `uri_to_iri`, `iri_to_uri`,
/// `escape_uri_path`, `filepath_to_uri`, `urlsafe_base64_encode` /
/// `_decode` and friends. Pure `std`, so it is always compiled.
pub mod url_codec;

/// AWS-style canonical request signed with SHA-256, replay-bounded
/// by an X-Date tolerance window. See [`hmac_auth::HmacAuthLayer`].
#[cfg(feature = "hmac-auth")]
pub mod hmac_auth;

/// Standalone HS256 JWT — sign / verify / decode for magic links,
/// microservice tokens, third-party SSO. See [`jwt::encode`] +
/// [`jwt::decode`].
#[cfg(feature = "jwt")]
pub mod jwt;

/// Pluggable JTI revocation / single-use store. The default is in-memory;
/// implement [`jti_store::JtiStore`] to back it with Redis or a database
/// when you run more than one instance.
pub mod jti_store;

/// Multipart file upload helper — wraps axum's multipart extractor +
/// the [`storage::Storage`] trait. See [`uploads::save_uploads`] +
/// [`uploads::UploadConfig`].
#[cfg(feature = "uploads")]
pub mod uploads;

/// First-class `Media` model — database-backed file references with
/// direct-browser upload, CDN-aware URLs, soft delete and orphan purge.
/// See [`media::Media`] + [`media::MediaManager`].
#[cfg(feature = "media")]
pub mod media;

/// Multi-channel notifications — fan one notification out to mail / database /
/// log / broadcast channels. See [`notifications::notify`].
#[cfg(feature = "notifications")]
pub mod notifications;

/// Background job queue with a worker pool — async work outside the request
/// lifecycle. In-memory by default; `jobs-postgres` adds the database-backed
/// queue. See [`jobs::JobQueue`].
#[cfg(feature = "jobs")]
pub mod jobs;

/// Pre-built auth flows — password reset, email verification, magic-link login.
/// See [`auth_flows::PasswordReset`] / [`auth_flows::EmailVerification`].
#[cfg(feature = "auth_flows")]
pub mod auth_flows;
/// Typed front end for the `tenancy::permissions` engine —
/// `has_perm_for_model::<T>` / `grant_role_perm_for_model::<T>` /
/// `model_codenames_for::<T>`. Ask for a permission by model type instead
/// of typing the codename string. Needs `tenancy`, which owns the tables.
#[cfg(feature = "tenancy")]
pub mod permissions;

/// Unified `RustangoError` enum + `From` impls for every framework error type.
/// Use in handlers: `async fn handler() -> RustangoResult<Json<X>> { ... }`.
mod error;
pub use error::{RustangoError, RustangoResult};

/// File storage backends — [`storage::Storage`] trait + LocalStorage + InMemoryStorage.
#[cfg(feature = "storage")]
pub mod storage;

/// Test client — fire HTTP requests against an `axum::Router` in tests
/// without binding a real socket. See [`test_client::TestClient`].
#[cfg(feature = "admin")]
pub mod test_client;

/// Live HTTP server for tests — Django's `LiveServerTestCase`. Binds an
/// `axum::Router` to a random localhost port on a background task. Use it
/// when in-process [`test_client::TestClient`] routing is not enough, for
/// example with Selenium, websockets, or code that reads the host header.
#[cfg(feature = "admin")]
pub mod test_server;

/// Typed environment variable readers — `required` / `with_default` /
/// `optional` / `list` / `duration_secs` / `duration_millis`.
pub mod env;

/// Internationalization (i18n) — translation lookups + Accept-Language negotiation.
/// See [`i18n::Translator`] and [`i18n::negotiate_language`].
pub mod i18n;

/// HTTP content negotiation — pick the best response format from the
/// client's `Accept` header. See [`content_negotiation::negotiate`].
pub mod content_negotiation;

/// ETag middleware — hashes 2xx response bodies, returns 304 when
/// `If-None-Match` matches. See [`etag::EtagLayer`].
#[cfg(feature = "admin")]
pub mod etag;

/// In-process scheduled task runner — fire async jobs at fixed intervals.
/// See [`scheduler::Scheduler`].
#[cfg(feature = "scheduler")]
pub mod scheduler;

/// API versioning — extract version from header / query / URL prefix.
/// See [`api_version::VersionStrategy`] and [`api_version::ApiVersion`].
#[cfg(feature = "admin")]
pub mod api_version;

/// Minimal RFC 4180 CSV writer — zero deps. See [`csv::CsvWriter`].
pub mod csv;

/// axum response wrapper for CSV exports — sets Content-Type +
/// optional Content-Disposition so browsers prompt a "Save as…"
/// download. See [`csv_response::CsvResponse`].
#[cfg(feature = "admin")]
pub mod csv_response;

/// Pluggable secrets backend — [`secrets::Secrets`] trait + [`secrets::EnvSecrets`]
/// + [`secrets::InMemorySecrets`].
#[cfg(feature = "secrets")]
pub mod secrets;

/// HTTP access log middleware — one tracing event per request with
/// method / path / status / duration / IP. See [`access_log::AccessLogLayer`].
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod access_log;

/// Ambient tenant identity for log lines — the per-request slot the
/// resolver fills and the access log reads back. Without a resolver the
/// slot stays empty and no tenant is logged. See [`tenant_log::record`] /
/// [`tenant_log::current`].
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod tenant_log;

/// Test fixture loader — seed a database from JSON files.
/// See [`fixtures::Fixture`].
pub mod fixtures;

/// Bulk-action runner — apply one operation to a set of selected PKs.
/// Works on all three backends. See [`bulk_actions::BulkActionRegistry`]
/// and the built-in actions.
#[cfg(feature = "tenancy")]
pub mod bulk_actions;

/// TOTP — RFC 6238 time-based one-time passwords for 2FA.
/// See [`totp::generate`] / [`totp::verify`] / [`totp::otpauth_url`].
#[cfg(feature = "totp")]
pub mod totp;

/// Text utilities — slugify, html_escape, truncate.
pub mod text;

/// Request ID middleware — assign per-request correlation IDs,
/// honor inbound `X-Request-Id` for end-to-end propagation. See
/// [`request_id::RequestIdLayer`].
#[cfg(feature = "admin")]
pub mod request_id;

/// IP allowlist / blocklist middleware. See [`ip_filter::IpFilterLayer`].
#[cfg(feature = "admin")]
pub mod ip_filter;

/// Host-header allowlist middleware — Django `ALLOWED_HOSTS` parity.
/// See [`host_validation::AllowedHostsLayer`].
#[cfg(feature = "admin")]
pub mod host_validation;

/// HTTP → HTTPS redirect middleware — Django `SECURE_SSL_REDIRECT`
/// + `SECURE_REDIRECT_EXEMPT` + `SECURE_PROXY_SSL_HEADER` parity.
/// See [`ssl_redirect::SslRedirectLayer`].
#[cfg(feature = "admin")]
pub mod ssl_redirect;

/// Request body size limit middleware — fast `Content-Length` rejection
/// returning structured `413 Payload Too Large` JSON. Complements
/// axum's per-extractor `DefaultBodyLimit`. See [`body_limit::BodyLimitLayer`].
#[cfg(feature = "admin")]
pub mod body_limit;

/// Per-request handler timeout — stops a handler that runs too long and
/// returns `504 Gateway Timeout`, so a stuck query or external call cannot
/// pile up. `Cli::with_settings_from_env()` wires it from
/// `Settings.server.request_timeout_secs`. See
/// [`request_timeout::RequestTimeoutLayer`].
#[cfg(feature = "admin")]
pub mod request_timeout;

/// Real-IP extraction for apps behind a trusted reverse proxy. Reads
/// `X-Forwarded-For`, `X-Real-IP`, `CF-Connecting-IP` or RFC 7239
/// `Forwarded` and puts the client IP in the request extensions. See
/// [`real_ip::RealIpLayer`].
#[cfg(feature = "admin")]
pub mod real_ip;

/// Idempotency-key middleware (Stripe-shape) — replays a stored
/// successful response when a write request arrives with the same
/// `Idempotency-Key`. Backed by any [`cache::Cache`]. See
/// [`idempotency::IdempotencyLayer`].
#[cfg(all(feature = "admin", feature = "cache"))]
pub mod idempotency;

/// Maintenance-mode middleware — flip a shared flag and new requests get
/// 503 with `Retry-After`, while requests already running finish. Use it
/// to drain traffic before a deploy or migration. See
/// [`maintenance::MaintenanceFlag`] + [`maintenance::MaintenanceLayer`].
#[cfg(feature = "admin")]
pub mod maintenance;

/// Trailing-slash redirect middleware — canonicalize URL paths
/// (Django `APPEND_SLASH` / Rails `trailing_slash` shape).
/// See [`trailing_slash::TrailingSlashLayer`].
#[cfg(feature = "admin")]
pub mod trailing_slash;

/// Static file serving — read files from a directory with sensible
/// `Content-Type`, `Cache-Control`, `Last-Modified`, and 304 support.
/// See [`static_files::StaticFiles`] + [`static_files::static_router`].
#[cfg(feature = "admin")]
pub mod static_files;

/// HTTP method override — rewrite POST to PUT/PATCH/DELETE from the
/// `X-HTTP-Method-Override` header or a `_method` form field, so HTML forms
/// can drive REST routes. See [`method_override::MethodOverrideLayer`].
#[cfg(feature = "admin")]
pub mod method_override;

/// RFC 7807 "Problem Details for HTTP APIs" — standardized error
/// responses with `application/problem+json`. Sister to
/// [`api_errors`]. See [`problem_details::ProblemDetails`].
#[cfg(feature = "admin")]
pub mod problem_details;

/// Feature flags / killswitches backed by [`cache::Cache`] —
/// boolean killswitch + per-user override + stable percentage rollout.
/// See [`feature_flags::FeatureFlags`].
#[cfg(feature = "cache")]
pub mod feature_flags;

/// Distributed locks backed by [`cache::Cache`] — only one worker runs a
/// task at a time. A TTL recovers the lock after a crash, and release is
/// token-checked. See [`distributed_lock::DistributedLock`].
#[cfg(feature = "cache")]
pub mod distributed_lock;

/// Server-side sessions backed by [`cache::Cache`] — opaque cookie
/// ID, server-stored bag of typed values, revocable per-session.
/// See [`sessions::Session`] + [`sessions::SessionStore`].
#[cfg(feature = "sessions")]
pub mod sessions;

/// Prometheus-format metrics — counters + histograms exposed at
/// `/metrics`. Pure-Rust, no Prometheus client crate.
/// See [`metrics::MetricsRegistry`] + [`metrics::metrics_router`].
pub mod metrics;

/// Per-request `tracing` span with OpenTelemetry field names and
/// `traceparent` header propagation. Add `tracing_opentelemetry::layer()`
/// to your subscriber to get distributed tracing. See
/// [`tracing_layer::TracingLayer`].
#[cfg(feature = "admin")]
pub mod tracing_layer;

/// `Server-Timing` header middleware — surface per-request stage
/// durations to the browser DevTools "Network" panel. See
/// [`server_timing::ServerTimingLayer`] + [`server_timing::Timings`].
#[cfg(feature = "admin")]
pub mod server_timing;

/// Webhook signature verification (HMAC-SHA256). See [`webhook::verify_signature`].
#[cfg(feature = "webhook")]
pub mod webhook;

/// Outbound webhook delivery — POSTs HMAC-signed JSON through the
/// background job queue, so retries with backoff come for free. See
/// [`webhook_delivery::WebhookSubscription`].
#[cfg(feature = "webhook-delivery")]
pub mod webhook_delivery;

/// Standardized API error responses. See [`api_errors::ApiError`].
#[cfg(feature = "admin")]
pub mod api_errors;

/// Generic API key generation + verification (argon2id-hashed).
/// See [`api_keys::generate_key`] / [`api_keys::verify_key`].
#[cfg(feature = "api_keys")]
pub mod api_keys;

/// Generic password hash/verify + strength heuristic. See [`passwords::hash`].
#[cfg(feature = "passwords")]
pub mod passwords;

/// Passkey / WebAuthn — credential storage
/// ([`passkey::WebauthnCredential`] plus the store API) and the
/// registration / assertion ceremonies, verified in pure Rust (CBOR +
/// ES256).
#[cfg(feature = "passkey")]
pub mod passkey;

/// Pagination helpers — RFC 5988 Link headers + cursor params.
/// See [`pagination::LinkHeaderBuilder`].
pub mod pagination;

/// Security headers middleware — HSTS / X-Frame-Options / nosniff /
/// Referrer-Policy / Permissions-Policy / CSP. See [`security_headers::SecurityHeadersLayer`].
#[cfg(feature = "admin")]
pub mod security_headers;

/// Per-request CSP nonce middleware — makes a fresh random nonce per
/// request, exposes it as `Extension<Nonce>`, and replaces
/// `'nonce-__RUSTANGO_NONCE__'` in the CSP header, so inline
/// `<script nonce="...">` tags pass a strict CSP. See
/// [`csp_nonce::CspNonceLayer`].
#[cfg(feature = "csp-nonce")]
pub mod csp_nonce;

/// Signed URL helpers — HMAC-SHA256 with optional expiry.
/// See [`signed_url::sign`] / [`signed_url::verify`].
#[cfg(feature = "signed_url")]
pub mod signed_url;

/// First-run welcome page — confidence signal that rustango is wired up.
/// Mount under `/` while bootstrapping; replace once you have content.
/// See [`welcome::welcome_router`].
#[cfg(feature = "admin")]
pub mod welcome;

/// Debug profiling panel at `/__debug__/` — Telescope/Debug-Toolbar-shape.
/// **DEV ONLY** — captures per-request telemetry. See [`debug_panel`].
#[cfg(all(feature = "admin", feature = "tenancy"))]
pub mod debug_panel;

/// Browser auto-reload — refreshes pages when the server restarts.
/// **DEV ONLY** — pairs with `cargo watch -x run`. See [`livereload`].
#[cfg(all(feature = "admin", feature = "tenancy"))]
pub mod livereload;

/// One-call tracing-subscriber setup. See [`logging::setup`] / [`logging::Setup`].
pub mod logging;

/// Per-account login lockout — defends against credential stuffing.
/// Cache-backed counter + lock flag. See [`account_lockout::Lockout`].
#[cfg(feature = "cache")]
pub mod account_lockout;

/// Broadcast event bus — fan-out for SSE / WebSocket / signal-driven push.
/// See [`sse::EventBus`].
#[cfg(feature = "sse")]
pub mod sse;

/// WebSocket handler scaffold — fan-out via [`sse::EventBus`] with auto
/// JSON encode/decode + keep-alive ping. See [`ws::WsHub`] +
/// [`ws::ws_handler`].
#[cfg(feature = "websocket")]
pub mod ws;

/// OAuth2 / OIDC social login. Works with plain OAuth2 providers (GitHub,
/// Discord) and OIDC ones (Google, Microsoft, Keycloak) through their
/// `/userinfo` endpoint. Per-tenant config via [`oauth2::OAuth2Registry`];
/// the optional axum router is [`oauth2::router`] (needs `admin`).
#[cfg(feature = "oauth2")]
pub mod oauth2;

/// SSO (OpenID Connect / social OAuth) login core, independent of the
/// admin. Holds the shared handshake pieces (`ResolvedSso`,
/// `build_provider`, `verified_email`, …) and the DB-backed
/// [`sso::SsoProvider`] model. The admin, the tenant console and member
/// auth all build on it; the `admin-sso` feature adds the admin login
/// wiring.
#[cfg(feature = "sso")]
pub mod sso;

/// Model Context Protocol (MCP) server — expose tools, prompts and
/// resources to external agents over JSON-RPC 2.0 and Streamable HTTP.
/// Mount with [`mcp::router`] (single tenant) or [`mcp::tenant_router`].
#[cfg(feature = "mcp")]
pub mod mcp;

/// Opinionated HTTP client — `reqwest` wrapper with sane timeouts,
/// retry on idempotent verbs / transient failures, default User-Agent.
/// See [`http_client::HttpClient`].
#[cfg(feature = "http-client")]
pub mod http_client;

/// HTTP `QUERY` method routing (RFC 10008) — `query(handler)` and
/// `.query()` chaining on axum's `MethodRouter`. See
/// [`http_query::QueryRouterExt`].
#[cfg(feature = "admin")]
pub mod http_query;

/// Method-adaptive params extractor — [`params::Params`] reads the query
/// string on GET/HEAD and the body on QUERY, so one handler serves both.
#[cfg(feature = "admin")]
pub mod params;

/// Response compression middleware — gzip and deflate. Honors
/// `Accept-Encoding` and skips bodies that are already compressed. See
/// [`compression::CompressionLayer`].
#[cfg(feature = "compression")]
pub mod compression;

/// OpenAPI 3.1 spec builder + Swagger UI / Redoc viewer routes.
/// Hand-build a spec with [`openapi::OpenApiSpec`] and mount it under
/// [`openapi::router::openapi_router`] (requires the `admin` feature).
#[cfg(feature = "openapi")]
pub mod openapi;

#[cfg(feature = "tenancy")]
pub mod tenancy;

/// Per-request extractors for handlers — tenancy-aware DI. Ships
/// [`extractors::Tenant`], [`extractors::SessionUser`], and
/// [`extractors::SessionOperator`].
#[cfg(feature = "tenancy")]
pub mod extractors;

/// DRF-style ModelViewSet — five REST endpoints for any [`Model`] table in
/// about five lines. Runs on all three backends, including the
/// `.serializer()` render extension. See [`viewset::ViewSet`].
///
/// `router(prefix, &PgPool)` keeps a PG-typed pool argument for source
/// compatibility. `tenant_router(prefix)` works on every dialect and needs
/// the `tenancy` feature; the static-pool CRUD ViewSet needs only `admin`.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod viewset;

/// Shared list-endpoint query-parameter helpers — one place that owns the
/// reserved-key skip list, `?ordering=` parsing and `?page_size=` clamping,
/// so `viewset::handle_list` and `template_views::ListView` cannot drift
/// apart.
pub mod list_params;

/// Generic class-based views for HTML templates (Django-shape) —
/// `ListView`, `DetailView`, `CreateView`, `UpdateView`, `DeleteView` over
/// `#[derive(Model)]` schemas, rendered with Tera. The HTML sibling of
/// [`viewset`]. Runs on all three backends. See [`template_views`] for
/// usage and the Tera context shape.
#[cfg(feature = "template_views")]
pub mod template_views;

/// Django-shape DEBUG template-error overlay — see [`template_debug`].
#[cfg(feature = "_tera")]
pub mod template_debug;

/// Django-shape template context processors — see
/// [`template_context_processors`].
#[cfg(feature = "_tera")]
pub mod template_context_processors;

/// Django-shape custom template filters and functions — see
/// [`template_extensions`].
#[cfg(feature = "_tera")]
pub mod template_extensions;

/// Django-shape view shortcuts — `get_object_or_404` / `get_list_or_404`
/// / `render` / `redirect`. See [`shortcuts`].
#[cfg(feature = "template_views")]
pub mod shortcuts;

/// Django `humanize` template filters — `intcomma`, `intword`,
/// `naturalsize`, `ordinal`, `apnumber`, `naturaltime`, `naturalday`.
/// See [`humanize`].
#[cfg(feature = "template_views")]
pub mod humanize;

/// Django-shape number formatter — `numberformat::format(value,
/// decimal_sep, decimal_pos, grouping, thousand_sep)`. Thousands
/// grouping, per-locale separators, optional fixed decimal width.
pub mod numberformat;

/// `django.utils.timesince` parity — [`timesince`](timesince::timesince) /
/// [`timeuntil`](timesince::timeuntil), with Django's `depth` control
/// ("4 days, 6 hours"). The Tera filter wrappers live in [`humanize`].
pub mod timesince;

/// Django `defaultfilters` template filters — `pluralize`,
/// `truncatewords`, `linebreaks`, `default_if_none`. See
/// [`default_filters`].
#[cfg(feature = "template_views")]
pub mod default_filters;

/// Django-shape standalone validators — `validate_email`,
/// `validate_url`, `validate_slug`, `validate_min_length` /
/// `validate_max_length`, `validate_min_value` / `validate_max_value`.
/// See [`validators`].
pub mod validators;

/// Custom Manager / QuerySet-extension pattern — Django's
/// `PublishedManager(Manager)` / `QuerySet.as_manager()` written as a
/// Rust extension trait. See [`manager`] for worked examples.
pub mod manager;

/// Model-inheritance patterns — Django's `Meta.abstract`, multi-table and
/// proxy shapes in Rust. See [`inheritance`] for worked mappings.
pub mod inheritance;

/// Composite-primary-key patterns — Django 5.2's `CompositePrimaryKey`
/// mapped to an `Auto<i64>` surrogate plus `unique_together`. See
/// [`composite_pk`].
pub mod composite_pk;

/// Named URL reversal — Django's `reverse(name, params)` and
/// `get_absolute_url()`. Pair with [`register_url!`].
pub mod urls;

/// Django-shape random-string helpers — `get_random_string`,
/// `get_random_token_urlsafe`. CSPRNG-backed, so they suit session IDs,
/// reset tokens and verification codes.
#[cfg(feature = "_rand")]
pub mod random;

/// Django-shape base36 integer encoding — used in password reset
/// URLs and other URL-friendly opaque IDs. `int_to_base36(n)` +
/// `base36_to_int(s)` round-trip.
pub mod base36;

/// Django-shape base62 integer encoding — the `[0-9A-Za-z]` alphabet, for
/// short URL IDs. Like base36 but case-sensitive, so 6 bits per character.
pub mod base62;

/// Django-shape lorem ipsum generators — placeholder text for demos, test
/// fixtures and template scaffolds. `lorem::words(n)`,
/// `lorem::paragraphs(n)`, `lorem::sentence()`.
#[cfg(feature = "_rand")]
pub mod lorem;

/// Django-shape HTTP date parser and formatter — reads RFC 1123, RFC 850
/// and asctime forms per RFC 7231; writes IMF-fixdate.
/// `http_date::http_date(secs)` + `parse_http_date(s)`.
pub mod http_date;

/// Django-shape ISO 8601 date / time / datetime / duration parsers —
/// `dateparse::{parse_date, parse_time, parse_datetime, parse_duration}`.
pub mod dateparse;

/// Django-shape date format-character expander — turns a template format
/// string such as `{{ obj|date:"Y-m-d H:i" }}` into the matching output.
pub mod dateformat;

/// Django `django.utils.dates` parity — month / weekday name
/// lookups: `month_full(m)`, `month_abbr(m)`, `month_ap(m)`,
/// `weekday_full(d)`, `weekday_abbr(d)`. English-only by design
/// (i18n is a separate concern).
pub mod dates;

/// Django-shape value signer — `signing::Signer::sign(value)` and
/// `signing::TimestampSigner` with a TTL. Use it for signed payloads such
/// as password reset tokens, magic links and signed cookies.
#[cfg(any(
    feature = "hmac-auth",
    feature = "storage-s3",
    feature = "signed_url",
    feature = "jwt",
))]
pub mod signing;

/// Django-shape `Set-Cookie` builder — `Cookie::new(name, value)
/// .path("/").max_age(secs).http_only().secure().same_site(...).build()`
/// returns an axum-ready `HeaderValue`. Use it instead of writing cookie
/// strings by hand.
pub mod cookies;

/// Django's `@require_http_methods` / `@require_GET` / `@require_POST` /
/// `@require_safe` as tower middleware. A method that is not allowed gets
/// `405 Method Not Allowed` with an `Allow:` header listing the rest.
#[cfg(feature = "_axum")]
pub mod http_methods;

/// Django messages framework — `messages.success/info/warning/error/debug`
/// flash storage in a signed cookie.
#[cfg(feature = "_signing")]
pub mod messages;

/// Django-shape access decorators — `login_required` middleware and
/// `?next=` round-trip helpers.
#[cfg(feature = "_axum")]
pub mod auth_decorators;

/// Pluggable `AUTHENTICATION_BACKENDS` chain — register an ordered list of
/// [`auth_backends::AuthBackend`] impls; the chain returns the first
/// non-`None` result. Ships `RemoteUserBackend` for SSO proxy setups.
#[cfg(feature = "_async_trait")]
pub mod auth_backends;

/// Pluggable `AUTH_PASSWORD_VALIDATORS` chain — run an ordered list of
/// [`password_validators::PasswordValidator`]s at signup and password
/// change. Built-ins: `MinimumLengthValidator`, `MaximumLengthValidator`,
/// `NumericPasswordValidator`, `UserAttributeSimilarityValidator`,
/// `CommonPasswordValidator`.
pub mod password_validators;

/// Pluggable `PASSWORD_HASHERS` chain with **upgrade-on-login**. The first
/// [`password_hashers::PasswordHasher`] in the list writes new hashes, and
/// every entry verifies its own format. A login that matches an older
/// hasher returns a fresh hash, so you can store the upgrade.
pub mod password_hashers;

/// Signed-cookie session primitives — an HMAC-SHA256 key wrapper and a
/// `sign(secret, msg)` helper, shared by every layer that sets a signed
/// cookie so the crypto lives in one place. See [`session::SessionSecret`].
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod session;

/// Graceful-shutdown signal handling — SIGINT **and** SIGTERM in one place,
/// so no serve path handles only half of them. See
/// [`shutdown::shutdown_signal`].
pub mod shutdown;

/// Interactive prompts for `manage` verbs — `ask(prompt)` reads a line and
/// `ask_password(prompt)` reads one without echoing. On non-TTY stdin both
/// return `Ok(None)`, so scripted callers still get the "missing required
/// flag" error.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod manage_interactive;

/// XML sitemap rendering — `django.contrib.sitemaps`. Write a `Sitemap`
/// impl (or pass a `Vec<SitemapEntry>`) and call
/// [`sitemaps::render_sitemap`]. For large sites,
/// [`sitemaps::render_sitemap_index`] points crawlers at child sitemaps.
pub mod sitemaps;

/// RSS 2.0 and Atom 1.0 feed rendering — `django.contrib.syndication`.
/// Build a [`syndication::Feed`] with channel metadata and
/// [`syndication::FeedItem`]s, then call [`syndication::render_rss`] or
/// [`syndication::render_atom`].
pub mod syndication;

/// Table-driven HTTP redirects — `django.contrib.redirects`. Build a
/// [`redirects::RedirectMap`] in code or from a CSV and mount
/// [`redirects::redirects_middleware`]. A matching request returns
/// 301/302 with the canonical URL in `Location`, query string kept.
#[cfg(feature = "_axum")]
pub mod redirects;

/// Static "flat pages" — `django.contrib.flatpages`. Build a
/// [`flatpages::FlatPageMap`] (path → [`flatpages::FlatPage`]) and mount
/// [`flatpages::flatpages_middleware`]. A matching request serves the page
/// body as `text/html`, or a content-type you choose. Tera wrapping is up
/// to you.
#[cfg(feature = "_axum")]
pub mod flatpages;

/// Django-shape test assertion helpers — `assert_contains` /
/// `assert_redirects` / `assert_status` / `assert_messages` over axum
/// responses. The `query_counter` submodule is ORM instrumentation that
/// `sql::executor` bumps on every query, so the module is always compiled
/// and the response assertions carry their own `_axum` gate.
pub mod test_assertions;

/// Tag-based test filtering — Django's `@tag('slow')` plus
/// `manage test --tag fast --exclude-tag slow`. Put `tags!("slow")` at the
/// top of a `#[test]` body and it skips itself when
/// `RUSTANGO_TEST_TAGS` / `RUSTANGO_TEST_EXCLUDE_TAGS` filter it out.
pub mod test_filter;

/// Class-level test fixtures — Django's `setUpTestData(cls)`. The
/// [`setup_test_data!`] / [`setup_test_data_async!`] macros build the
/// fixture once per test binary.
pub mod test_data;

/// Django-shape test factories — `factory_boy` parity. A
/// [`test_factory::Sequence`] counter plus the [`test_factory::Factory`]
/// trait with `build_batch`.
pub mod test_factory;

/// Test-only Settings overlay — Django's `@override_settings`. Installs a
/// per-task [`config::Settings`] overlay, so tests change configuration
/// without touching process-global state. [`test_settings::current`] reads
/// the overlay first and falls back to the real settings.
#[cfg(feature = "config")]
pub mod test_settings;

/// Test-time DB isolation — Django's `TestCase` transaction wrapping.
/// [`test_db::with_rollback`] runs an async closure in a transaction that
/// always rolls back, so one test's writes never reach the next.
pub mod test_db;

/// `manage dbshell` — spawn `psql` / `mysql` / `sqlite3` for the current
/// `DATABASE_URL`.
pub mod dbshell;

/// Django-style runserver. [`server::Builder`] owns the boilerplate a
/// tenancy app would otherwise rewrite: DB pool, resolver chain, host
/// dispatch, operator console, bind and serve.
///
/// The single-pool [`server::AppBuilder`] is available without `tenancy`;
/// the multi-tenant [`server::Builder`] is gated on it inside the module.
pub mod server;

/// Unified manage runner — one [`manage::Cli`] builder in place of
/// `src/main.rs` + `src/bin/manage.rs` boilerplate. The tenancy variant
/// appears with the `tenancy` feature. The `manage` feature is on by
/// default and pulls in a small axum + tokio surface, so bare-API projects
/// can use the dispatcher without `admin`.
#[cfg(feature = "manage")]
pub mod manage;

/// `#[rustango::main]` — the Django-shape `runserver` entrypoint. Wraps
/// `#[tokio::main]` and boots `tracing-subscriber` from `RUST_LOG`,
/// falling back to `info,sqlx=warn`.
#[cfg(feature = "runtime")]
pub use rustango_macros::main;

/// Internal re-exports for proc-macros that need to name third-party
/// crates without forcing the user to add them to their `Cargo.toml`.
/// Not part of the public API — names here may change between minors.
#[doc(hidden)]
#[cfg(feature = "runtime")]
pub mod __private_runtime {
    /// Lets `#[rustango::main]` resolve `tokio::main` through the rustango
    /// facade, so apps need no direct `tokio` dependency.
    pub use tokio;
    pub use tracing_subscriber;
}

/// Proc-macros crate, re-exported. End users normally reach
/// [`Model`] and [`embed_migrations`] directly via the facade rather
/// than naming `macros`.
pub use rustango_macros as macros;

/// Lets the `#[rustango(default_uuid_v7)]` derive arm reach
/// `Uuid::now_v7()` through the rustango facade, so consumer crates need
/// no direct `uuid` dependency. **Not part of the public API.**
#[doc(hidden)]
pub use uuid as __uuid;

/// Lets `tracing::warn!` calls emitted by `#[derive(Model)]` resolve
/// through the rustango facade. **Not part of the public API.**
#[doc(hidden)]
pub use tracing as __tracing;

/// Lets `chrono::Utc::now()` emitted by `#[derive(Model)]` on
/// `auto_now_add` / `auto_now` fields resolve through the facade.
/// **Not part of the public API.**
#[doc(hidden)]
pub use chrono as __chrono;

/// Lets `serde_json::Value` / `to_value` emitted by `#[derive(Model)]`
/// on the audit and diff paths resolve through the facade.
/// **Not part of the public API.**
#[doc(hidden)]
pub use serde_json as __serde_json;

/// Lets the `serde::Serialize` impls emitted by `#[derive(Serializer)]`
/// resolve through the facade. **Not part of the public API.**
#[doc(hidden)]
pub use serde as __serde;

/// Lets `rust_decimal::Decimal` emitted by `#[derive(Model)]` at the
/// Decimal-FK default site resolve through the facade.
/// **Not part of the public API.**
#[doc(hidden)]
pub use rust_decimal as __rust_decimal;

/// Lets the `axum::Router`-returning method emitted by
/// `#[derive(ViewSet)]` resolve through the facade. Gated on `admin`,
/// the feature that brings axum in. **Not part of the public API.**
#[cfg(feature = "admin")]
#[doc(hidden)]
pub use axum as __axum;

/// `#[derive(Model)]` — populates the `inventory` registry the admin
/// walks, generates `objects()` / typed columns / `insert` / `delete`
/// / `save`.
pub use rustango_macros::Model;

/// Re-exported for `register_admin_computed!` and any user code that
/// wants to submit inventory entries via macros that reference
/// `$crate::inventory`. The runtime indirection costs nothing —
/// `inventory` is a thin wrapper around static `ctor`-style entries.
#[doc(hidden)]
pub use inventory;

/// Server-assigned PK wrapper. `id: Auto<i64>` → `BIGSERIAL`. See
/// [`sql::Auto`] for details.
pub use sql::Auto;

/// Bake every migration file in a directory into the binary at
/// compile time, for shipping a single-binary distribution. Pair
/// with [`migrate::migrate_embedded`].
pub use rustango_macros::embed_migrations;

/// `Q!()` — Django-shape filter syntax compile-time-resolved against
/// typed columns. Each invocation expands to the equivalent typed-column
/// method call, so field-name typos fail the build. See
/// [`rustango_macros::Q`] for the supported lookup suffixes.
///
/// ```ignore
/// use rustango::{Q, core::Column as _};
///
/// User::objects()
///     .where_(Q!(User.email__icontains = "alice"))
///     .fetch(&pool).await?;
/// // → WHERE "email" ILIKE $1   ($1 = "%alice%")
/// ```
#[allow(non_upper_case_globals)]
pub use rustango_macros::Q;

/// `#[derive(Form)]` — implements [`forms::Form`] so a struct can be
/// parsed from an HTTP form payload with multi-error validation.
/// Re-exported only when the `forms` feature is on.
#[cfg(feature = "forms")]
pub use rustango_macros::Form;

/// `#[derive(ViewSet)]` — generates a `router(prefix, pool) -> axum::Router`
/// associated method on a marker struct, wiring the full CRUD ViewSet in one
/// annotation. Re-exported under `admin` or `tenancy` (the generated
/// `router(...)` expands to `__axum::Router`, which `admin` provides); the
/// generated `router(...)` works without tenancy, and `tenant_router(...)`
/// adds the multi-tenant path when `tenancy` is also enabled.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub use rustango_macros::ViewSet;

/// `#[derive(Serializer)]` — implements [`serializer::ModelSerializer`] on a
/// struct, generating `from_model`, a custom `serde::Serialize` (respecting
/// `write_only`), and `writable_fields`. Re-exported when the `serializer`
/// feature is on.
#[cfg(feature = "serializer")]
pub use rustango_macros::Serializer;
