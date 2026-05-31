//! rustango — a Django-shaped, batteries-included web framework for Rust.
//!
//! ORM with auto-migrations, auto-admin, multi-tenancy, sessions +
//! JWT + OAuth2/OIDC + HMAC auth, DRF-style serializers + viewsets,
//! signals, caching, media (S3/R2/B2/MinIO), email pipeline,
//! background jobs, scheduled tasks, OpenAPI 3.1 auto-derive, every
//! standard middleware. Postgres + MySQL + **SQLite** through the
//! same `&Pool` API, opt-in via Cargo features.
//!
//! ```toml
//! [dependencies]
//! rustango = "0.41"                                       # Postgres (default)
//! rustango = { version = "0.41", features = ["sqlite"] }  # SQLite
//! rustango = { version = "0.41", features = ["mysql"] }   # MySQL 8.0+
//! ```
//!
//! ## What's new in v0.41 (May 2026) — Tier 1 ORM gap-closure batch
//!
//! Django-shape syntax with Rust-shape compile-time safety. Ten ORM
//! tickets across 14 PRs ([epic #273]):
//!
//! - **[`Q!()`] compile-time macro** ([#269]) — `Q!(User.email__icontains
//!   = "alice")` expands at parse time to the typed-column call.
//!   Typo'd field names fail the build.
//! - **[`query::Q`] runtime composable predicate** ([#263]) — for
//!   dynamic filter trees (admin chips, REST query params). Operator
//!   overloads: `&` / `|` / `^` / `!`.
//! - **[`QuerySet::distinct_on`]** ([#264]) — PG `DISTINCT ON` native +
//!   `ROW_NUMBER()` portable fallback on MySQL/SQLite. The "latest per
//!   group" pattern, tri-dialect.
//! - **[`Model::bulk_upsert_pool`] / [`Model::bulk_insert_or_ignore_pool`]**
//!   ([#267]) — Django `bulk_create(update_conflicts=True)` /
//!   `(ignore_conflicts=True)` across PG, MySQL, and SQLite.
//! - **`#[rustango(unique_when(...))]`** ([#265]) — partial unique
//!   indexes (Django `UniqueConstraint(condition=Q(...))`). PG/SQLite
//!   native; MySQL warns + falls back.
//! - **DB functions batch 1** ([#266]) — Cast, LPad, RPad, MD5, SHA1,
//!   SHA256, Position, Repeat, Reverse, Sign, Mod, Power, Sqrt — per-
//!   dialect emission with clean `NotSupported` errors on SQLite for
//!   the genuinely-missing items (hashes, Reverse, Power/Sqrt without
//!   `SQLITE_ENABLE_MATH_FUNCTIONS`).
//! - **`AggregateBuilder::alias()`** ([#268]) — Django 3.2 non-
//!   projected annotation. Filter/order by a derived aggregate without
//!   paying the column-decode cost.
//! - **`#[rustango(manager(ext = "..."))]`** ([#271]) — derive emits
//!   the custom-manager extension trait next to the model.
//! - **[`sql::explain_pool`]** ([#272]) — tri-dialect query plan
//!   helper. PG `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS)`, MySQL
//!   `EXPLAIN ANALYZE` / `FORMAT=TREE` / `FORMAT=JSON`, SQLite
//!   `EXPLAIN QUERY PLAN`.
//! - **PG-typed legacy executor deleted** ([#270], 4 waves) — the
//!   bi-API confusion is gone. `Fetcher`/`Counter`/`Updater`/`Deleter`
//!   extension traits + the `&PgPool` standalone fns +
//!   `transaction`/`count_rows`/`raw_execute`/`bulk_update` are
//!   removed. The `_on` family moved to a `#[doc(hidden)]` module for
//!   macro use only. **Migration**: drop the trait imports, switch
//!   `.fetch(pool)` → `.fetch_on(pool)` (inherent on QuerySet),
//!   `.delete(pool)` → `.delete_on(pool)`, `.execute(pool)` (on
//!   UpdateBuilder) → `.execute_on(pool)`. Use the `_pool` family for
//!   tri-dialect calls.
//!
//! Full ticket index: PRs #274–#286.
//!
//! [`Q!()`]: rustango_macros::Q
//! [`query::Q`]: crate::query::Q
//! [`QuerySet::distinct_on`]: crate::query::QuerySet::distinct_on
//! [`Model::bulk_upsert_pool`]: crate::core::Model
//! [`Model::bulk_insert_or_ignore_pool`]: crate::core::Model
//! [`sql::explain_pool`]: crate::sql::explain_pool
//! [epic #273]: https://github.com/ujeenet/rustango/issues/273
//! [#263]: https://github.com/ujeenet/rustango/pull/279
//! [#264]: https://github.com/ujeenet/rustango/pull/280
//! [#265]: https://github.com/ujeenet/rustango/pull/282
//! [#266]: https://github.com/ujeenet/rustango/pull/277
//! [#267]: https://github.com/ujeenet/rustango/pull/281
//! [#268]: https://github.com/ujeenet/rustango/pull/274
//! [#269]: https://github.com/ujeenet/rustango/pull/276
//! [#270]: https://github.com/ujeenet/rustango/issues/270
//! [#271]: https://github.com/ujeenet/rustango/pull/278
//! [#272]: https://github.com/ujeenet/rustango/pull/275
//!
//! ## What's new in v0.40 (May 2026)
//!
//! - **Admin session auth without tenancy** ([epic #253]) — bare
//!   [`admin`] now ships a styled `/login` form, signed-cookie
//!   sessions ([`session::SessionSecret`]), sidebar Logout, password
//!   change at `/account/password`, [`manage create-admin`][admin-cli]
//!   CLI verb, and `is_superuser` gating. Opt in via
//!   [`admin::Builder::with_session_auth`]. Same signing key works
//!   across [`tenancy::session`] + [`admin::session`] (different
//!   cookie names + payloads so cookies never cross-decode).
//! - **GenericForeignKey ergonomics + admin inlines** ([epic #246])
//!   — `#[rustango(generic_fk(name, ct_column, pk_column))]` emits
//!   typed `target_pool()` accessor + `set_target_for::<T>()`
//!   setter. Admin list view collapses the `(ct_id, object_pk)` pair
//!   into one clickable target link. New
//!   [`register_admin_inline_generic!`] renders polymorphic children
//!   as inline panels on the parent's detail + edit pages.
//!   ContentType `<select>` picker replaces raw integer inputs on
//!   the standalone create/edit form.
//! - **Field [`help_text`]** — `#[rustango(help_text = "…")]` on any
//!   field renders a muted caption below the admin form input.
//!   The string lives on [`core::FieldSchema::help_text`] so future
//!   surfaces (DRF schemas, OpenAPI descriptions) can read the same
//!   source.
//! - **Shared `.btn` family** — admin + operator-console buttons,
//!   links, sidebar Logout, inline-panel row links all look like
//!   one product. Consistent across bare admin / tenant admin /
//!   operator console.
//! - **Reusable foundations** — [`session::SessionSecret`] +
//!   [`session::sign`] (HMAC primitive) and
//!   [`manage_interactive`] (TTY prompts) promoted to the crate
//!   root so the bare admin can reuse them without `tenancy`
//!   compiled in.
//! - **Runnable demo**: [`examples/gfk_demo`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/gfk_demo)
//!   exercises every new admin + GFK surface end-to-end on SQLite.
//!
//! [epic #246]: https://github.com/ujeenet/rustango/issues/246
//! [epic #253]: https://github.com/ujeenet/rustango/issues/253
//! [admin-cli]: crate::manage::Cli
//! [`help_text`]: crate::core::FieldSchema::help_text
//! [`register_admin_inline_generic!`]: crate::register_admin_inline_generic
//! [`manage_interactive`]: crate::manage_interactive
//!
//! Defaults pull `postgres`, `admin`, and `runserver` (the bi-dialect
//! single-pool [`server::AppBuilder`]). Add `"tenancy"` for the
//! multi-tenant resolver / pools / per-tenant auth. Tenancy is
//! tri-dialect since v0.33: [`tenancy::TenantPools`] is generic over
//! the backend, database-mode tenants work on every backend, and
//! schema-mode stays Postgres-only by language semantics. Add
//! `"mysql"` or `"sqlite"` for additional backends — the same
//! `#[derive(Model)]` types and `_pool` ORM API work against any of
//! them. Drop `default-features` for the bare ORM (no axum, no Tera).
//!
//! # Quick start
//!
//! ```bash
//! cargo install cargo-rustango
//! cargo rustango new myblog                 # default: ORM + admin
//! cargo rustango new myapi --template api   # JSON-only, no admin
//! cargo rustango new shop --template tenant # multi-tenancy + operator console
//! ```
//!
//! Then:
//!
//! ```bash
//! cd myblog
//! cp .env.example .env                      # set DATABASE_URL
//! cargo run -- migrate                      # apply bootstrap migrations
//! cargo run                                 # http://localhost:8080
//! ```
//!
//! # Unified manage runner (since v0.16)
//!
//! Since v0.16 there is **one binary** that serves HTTP and dispatches
//! manage commands. `cargo run` starts the server; `cargo run -- <verb>`
//! runs a CLI command. A scaffolded `src/main.rs` looks like:
//!
//! ```ignore
//! #[rustango::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let _ = dotenvy::dotenv();
//!     rustango::manage::Cli::new()
//!         .api(myblog::urls::router())
//!         .tenancy()                                            // optional, with `tenancy` feature
//!         .migrations_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
//!         .run()
//!         .await
//! }
//! ```
//!
//! Common manage verbs (every command supports `--help`):
//!
//! | Group | Verbs |
//! |---|---|
//! | Migrations | `makemigrations`, `migrate [target]`, `downgrade [N]`, `showmigrations`, `add-data-op` |
//! | Scaffolders | `startapp <name>`, `make:viewset`, `make:serializer`, `make:form`, `make:job`, `make:notification`, `make:middleware`, `make:test` |
//! | System | `about`, `check`, `check --deploy`, `version`, `docs` |
//! | Tenancy | `create-tenant`, `create-operator`, `create-user`, `list-tenants`, `create-api-key`, `grant-perm`, `revoke-perm`, `audit-cleanup` |
//!
//! # Full reference
//!
//! See the workspace [README](https://github.com/ujeenet/rustango)
//! for the complete feature matrix, ORM cookbook, viewset / serializer
//! recipes, multi-tenancy guide, and production checklist. The
//! [`examples/cookbook_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/cookbook_blog)
//! crate ships a runnable multi-tenant blog with one chapter per
//! feature surface.

// Lets `::rustango::core::Model` (emitted by the proc-macro) and
// `rustango::sql::Auto<i64>` (used in tenancy source code carried
// over from the pre-collapse layout) resolve to ourselves without
// rewriting either.
extern crate self as rustango;

// ---------------------------------------------------------------- bi-dialect macro support
//
// The `#[derive(Model)]` proc-macro emits an unconditional call to
// [`__impl_my_from_row!`]. The macro_rules below has two definitions
// gated on rustango's own `mysql` feature:
//
// - With `mysql` enabled: expands to a real
//   `impl<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> for $struct`,
//   so user models can be SELECT-decoded against either backend.
// - With `mysql` disabled: expands to nothing.
//
// This sidesteps the cross-crate feature-gating problem — the proc
// macro can't see the user's `Cargo.toml` at expansion time, but
// macro_rules expansion happens *after* the user's crate has resolved
// rustango's feature set, so the right arm fires automatically. Users
// don't have to add a `mysql = ["rustango/mysql"]` shim feature to
// their own crate.

#[doc(hidden)]
#[cfg(feature = "mysql")]
#[macro_export]
macro_rules! __impl_my_from_row {
    // The caller passes its own `row` identifier (`|$row|`) so the
    // function parameter and the body's `row.try_get(...)` references
    // share a single macro hygiene context. Without this, `$row` defined
    // by the macro_rules is invisible to body tokens minted in the
    // proc-macro's hygiene context, and the body fails name resolution.
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

// ---- select_related's aliased-row decoder for MySQL (batch 8) ----
//
// `Model::__rustango_from_aliased_row(row, prefix)` is the per-model
// helper the executor calls when decoding LEFT-JOINed columns into a
// FK target. v0.23.0-batch8 adds a MySQL-typed counterpart via the
// same hygiene-aware closure pattern as `__impl_my_from_row!`.

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
// The PG `LoadRelated` impl stays as-is. The MySQL counterpart
// implements the new [`LoadRelatedMy`] trait (added in batch 8 to
// the rustango sql module) — same shape, MySqlRow-typed.

#[doc(hidden)]
#[cfg(feature = "mysql")]
#[macro_export]
macro_rules! __impl_my_load_related {
    // The proc-macro emits arms that mutate `self.<field_ident>`. The
    // earlier version omitted `$self` from the header on the
    // (incorrect) assumption that `self` as a keyword bypasses macro
    // hygiene. In practice `self` IS hygiene-tracked through
    // macro_rules, and the proc-macro's emitted token comes from a
    // different hygiene context than the `&mut self` parameter in
    // the expanded fn — so the compile fails with "expected value,
    // found module `self`" the moment any FK arm is generated.
    //
    // Fix: capture `$self` explicitly. The proc-macro now passes it
    // through and the macro_rules substitutes it where the arms use
    // it.
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident| {
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
                match $field {
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
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident| {
        $($arms:tt)*
    }) => {};
}

// ---- SQLite counterparts to the three MySQL macros above ----
//
// Phase 3 of #37 ("SQLite ORM backend"). Same hygiene-aware closure
// pattern as the MySQL trio, gated on `sqlite` instead of `mysql`.
// The proc-macro emits unconditional calls to each; the macro_rules
// definitions below collapse to nothing when the feature is off.

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
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident| {
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
                match $field {
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
    ($struct:ty, |$self_:ident, $row:ident, $field:ident, $alias:ident| {
        $($arms:tt)*
    }) => {};
}

pub mod audit;
/// ContentType framework (Django-shape) — runtime handle for
/// "any registered model" used by permissions, generic foreign
/// keys, soft-FK prefetch, audit-history admin panels, etc.
/// Sub-slice F.1 of the v0.15.0 plan.
pub mod contenttypes;
pub mod core;
pub mod migrate;
pub mod query;
pub mod sql;

/// Soft-delete query helpers — `active_filter` / `trashed_filter` /
/// `compose_with_active` / `soft_delete` / `restore` / `purge` for any
/// model carrying `#[rustango(soft_delete)]`. See [`soft_delete`].
///
/// v0.38 — fully tri-dialect via `&crate::sql::Pool`.
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
/// [`cache_fragment::cached_render`] is a handler-side wrapper that
/// consults the cache before invoking a compute closure. Tera lacks
/// the block-tag extension API a 1:1 port would need; the
/// handler-side helper composes naturally with axum and passes the
/// cached HTML through the template context. Issue #16.
#[cfg(feature = "cache")]
pub mod cache_fragment;

/// Per-view caching tower layer + `Cache-Control` / `Vary` header
/// builders — Django's `@cache_page` / `@cache_control` / `@vary_on_*`
/// analogs (issue #55). Behind the `cache-page` feature.
#[cfg(feature = "cache-page")]
pub mod cache_page;

/// Django-shape model signals — [`signals::connect_post_save`] etc.
/// Receivers register globally per model type and run sequentially.
#[cfg(feature = "signals")]
pub mod signals;

/// Domain event bus — typed pub-sub for application-level events
/// decoupled from the ORM. Use this for cross-component fanout
/// (`OrderPlaced` → mail + billing + audit handlers); use
/// [`signals`] for per-Model lifecycle hooks. See [`events::EventBus`].
pub mod events;

/// CORS middleware — [`cors::CorsLayer`] for axum routers.
#[cfg(feature = "admin")]
pub mod cors;

/// Token-bucket rate limiting middleware — [`rate_limit::RateLimitLayer`].
/// Per-IP, per-header, or global. Returns 429 with `Retry-After` when exhausted.
#[cfg(feature = "admin")]
pub mod rate_limit;

/// Cache-backed rate limiting middleware — fixed-window counter via the
/// [`cache::Cache`] trait. Pair with `RedisCache` for distributed
/// enforcement across multiple processes / replicas. See
/// [`rate_limit_cache::CacheRateLimitLayer`].
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

/// Send email off the request path via the [`crate::jobs`] queue.
/// Pairs the [`email::Mailer`] trait with the queue's retry-with-
/// backoff so transient SMTP failures don't blow up handlers.
/// See [`email_jobs::register_email_job`] + [`email_jobs::dispatch_email`].
#[cfg(all(feature = "email", feature = "jobs"))]
pub mod email_jobs;

/// Laravel-shape `Mailable` trait — declare an email type as a
/// struct that owns its template + recipient logic. Pairs with
/// [`email_templates::EmailRenderer`] (rendering) and
/// [`email_jobs`] (off-request delivery). See [`mailable::Mailable`].
#[cfg(all(feature = "email", feature = "admin"))]
pub mod mailable;

/// JSON:API v1.1 response envelope adapter — wrap a flat
/// `serde_json::Value` (typical [`serializer`] output) into the
/// `{"data": {"type", "id", "attributes": {...}}}` shape that
/// JSON:API clients expect. See [`jsonapi::to_resource`] +
/// [`jsonapi::to_collection`].
pub mod jsonapi;

/// HMAC-signed request authentication for service-to-service traffic.
/// Shared HMAC-SHA256 / SHA-256 / hex-encoding primitives. Internal —
/// users wanting raw HMAC pull in `hmac` + `sha2` themselves.
#[cfg(any(
    feature = "hmac-auth",
    feature = "storage-s3",
    feature = "signed_url",
    feature = "jwt",
))]
pub(crate) mod crypto;

/// Tiny `application/x-www-form-urlencoded` decoder shared by
/// [`signed_url`], [`auth_flows`], and [`tenancy::admin`]. Internal.
#[cfg(any(feature = "signed_url", feature = "auth_flows", feature = "tenancy",))]
pub(crate) mod url_codec;

/// AWS-style canonical request signed with SHA-256, replay-bounded
/// by an X-Date tolerance window. See [`hmac_auth::HmacAuthLayer`].
#[cfg(feature = "hmac-auth")]
pub mod hmac_auth;

/// Standalone HS256 JWT — sign / verify / decode for magic links,
/// microservice tokens, third-party SSO. See [`jwt::encode`] +
/// [`jwt::decode`].
#[cfg(feature = "jwt")]
pub mod jwt;

/// Pluggable JTI revocation / single-use store. v0.47 extracts the
/// in-memory `Mutex<HashMap>` that was hard-coded into
/// [`tenancy::jwt_lifecycle::JwtLifecycle`] and the impersonation
/// handoff into a [`jti_store::JtiStore`] trait so multi-instance
/// deployments can plug in Redis / DB-backed stores.
pub mod jti_store;

/// Multipart file upload helper — wraps axum's multipart extractor +
/// the [`storage::Storage`] trait. See [`uploads::save_uploads`] +
/// [`uploads::UploadConfig`].
#[cfg(feature = "uploads")]
pub mod uploads;

/// First-class `Media` model — Postgres-backed file references with
/// direct-browser upload, CDN-aware URLs, soft delete + orphan purge.
/// See [`media::Media`] + [`media::MediaManager`].
#[cfg(feature = "media")]
pub mod media;

/// Multi-channel notifications — fan one notification out to mail / database /
/// log / broadcast channels. See [`notifications::notify`].
#[cfg(feature = "notifications")]
pub mod notifications;

/// Background job queue with worker pool — async work outside the request
/// lifecycle. Currently in-memory only. See [`jobs::JobQueue`].
#[cfg(feature = "jobs")]
pub mod jobs;

/// Pre-built auth flows — password reset, email verification, magic-link login.
/// See [`auth_flows::PasswordReset`] / [`auth_flows::EmailVerification`].
#[cfg(feature = "auth_flows")]
pub mod auth_flows;
/// Top-level permissions facade — typed convenience over the
/// existing `tenancy::permissions` engine. Adds
/// `has_perm_for_model::<T>` / `grant_role_perm_for_model::<T>` /
/// `model_codenames_for::<T>` so callers reach for permissions by
/// their `T: Model` type instead of hand-typing the codename
/// string. Sub-slice of v0.16.0 Option G. Requires the `tenancy`
/// feature (the underlying tables live in the tenancy bootstrap).
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

/// Live HTTP server for tests — Django's `LiveServerTestCase`. Binds
/// `axum::Router` to a random localhost TCP port and runs it on a
/// background task. Use when in-process [`test_client::TestClient`]
/// routing isn't enough (Selenium, websockets, code reading
/// scheme/host headers). Issue #39 partial.
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
#[cfg(feature = "admin")]
pub mod access_log;

/// Test fixture loader — seed a database from JSON files.
/// See [`fixtures::Fixture`].
pub mod fixtures;

/// Bulk-action runner — apply one operation to a set of selected PKs.
/// See [`bulk_actions::BulkActionRegistry`] + built-in actions.
///
/// v0.38 slice 6 — fully tri-dialect. [`BulkAction::run`] takes
/// `&crate::sql::Pool`; built-ins emit per-dialect DDL via
/// `dialect.quote_ident()` + `dialect.placeholder(n)` and bind
/// timestamps as `chrono::DateTime<Utc>` to dodge `NOW()` vs
/// `CURRENT_TIMESTAMP` differences.
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

/// Request body size limit middleware — fast `Content-Length` rejection
/// returning structured `413 Payload Too Large` JSON. Complements
/// axum's per-extractor `DefaultBodyLimit`. See [`body_limit::BodyLimitLayer`].
#[cfg(feature = "admin")]
pub mod body_limit;

/// Per-request handler timeout — kills handlers that exceed the
/// configured duration and returns `504 Gateway Timeout`. Caps the
/// blast radius of wedged DB queries / external HTTP calls. Wired
/// from `Settings.server.request_timeout_secs` automatically by
/// `Cli::with_settings_from_env()`. See
/// [`request_timeout::RequestTimeoutLayer`].
#[cfg(feature = "admin")]
pub mod request_timeout;

/// Real-IP extraction for apps behind a trusted reverse proxy
/// (Cloudflare / nginx / ELB / etc). Parses `X-Forwarded-For`,
/// `X-Real-IP`, `CF-Connecting-IP`, or RFC 7239 `Forwarded` and stuffs
/// the resolved client IP into request extensions. See
/// [`real_ip::RealIpLayer`].
#[cfg(feature = "admin")]
pub mod real_ip;

/// Idempotency-key middleware (Stripe-shape) — replays a stored
/// successful response when a write request arrives with the same
/// `Idempotency-Key`. Backed by any [`cache::Cache`]. See
/// [`idempotency::IdempotencyLayer`].
#[cfg(all(feature = "admin", feature = "cache"))]
pub mod idempotency;

/// Maintenance-mode middleware — return 503 with `Retry-After` from a
/// shared atomic flag, so an orchestrator can drain traffic before a
/// deploy / migration without killing in-flight requests. See
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

/// HTTP method override — rewrite POST → PUT/PATCH/DELETE based on
/// `X-HTTP-Method-Override` header or `_method` form field, so HTML
/// forms can drive REST routes. See [`method_override::MethodOverrideLayer`].
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

/// Distributed locks backed by [`cache::Cache`] — "only one worker
/// at a time runs this task" with TTL-based crash recovery and
/// token-checked release. See [`distributed_lock::DistributedLock`].
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

/// Per-request `tracing` span with W3C / OpenTelemetry-conventional
/// field names + `traceparent` header propagation. Hook
/// `tracing_opentelemetry::layer()` in your subscriber for free
/// distributed tracing. See [`tracing_layer::TracingLayer`].
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

/// Outbound webhook delivery — POSTs HMAC-signed JSON via the background
/// job queue (so retries with exponential backoff are free). See
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

/// Pagination helpers — RFC 5988 Link headers + cursor params.
/// See [`pagination::LinkHeaderBuilder`].
pub mod pagination;

/// Security headers middleware — HSTS / X-Frame-Options / nosniff /
/// Referrer-Policy / Permissions-Policy / CSP. See [`security_headers::SecurityHeadersLayer`].
#[cfg(feature = "admin")]
pub mod security_headers;

/// Per-request CSP nonce middleware — generates a fresh CSPRNG nonce
/// per request, makes it available via `Extension<Nonce>`, and
/// substitutes `'nonce-__RUSTANGO_NONCE__'` in the CSP header so
/// inline `<script nonce="...">` tags pass strict CSP. See
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

/// OAuth2 / OIDC swiss-knife — social login that works for both pure
/// OAuth2 (GitHub, Discord) and OIDC (Google, Microsoft, Keycloak)
/// providers via the `/userinfo` endpoint. Per-tenant via
/// [`oauth2::OAuth2Registry`]. Optional axum router under [`oauth2::router`]
/// (requires the `admin` feature).
#[cfg(feature = "oauth2")]
pub mod oauth2;

/// Opinionated HTTP client — `reqwest` wrapper with sane timeouts,
/// retry on idempotent verbs / transient failures, default User-Agent.
/// See [`http_client::HttpClient`].
#[cfg(feature = "http-client")]
pub mod http_client;

/// Response compression middleware — gzip + deflate via flate2.
/// Honors `Accept-Encoding`, skips already-compressed bodies and binary
/// content-types. See [`compression::CompressionLayer`].
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

/// DRF-style ModelViewSet — five REST endpoints for any [`Model`] table
/// in ~5 lines. See [`viewset::ViewSet`].
///
/// v0.38 — fully tri-dialect. Internal queries route through
/// `select_rows_as_json` + `insert_returning_pool` etc., so the
/// same ViewSet body runs on PG / MySQL / SQLite. The `.serializer()`
/// row-render extension stays PG-only because it decodes via
/// `T::from_row(&PgRow)`; non-PG ViewSets use the default field-level
/// JSON projection.
///
/// The `router(prefix, &PgPool)` (static-pool) entry point keeps its
/// PG-typed pool arg for source-compat; `tenant_router(prefix)`
/// works on every dialect via the generic Tenant<DB>.pool() path.
#[cfg(feature = "tenancy")]
pub mod viewset;

/// Generic class-based views for HTML templates (Django-shape) —
/// `ListView`, `DetailView`, `CreateView`, `UpdateView`, `DeleteView`
/// over `#[derive(Model)]` schemas, rendered through Tera. Sibling of
/// [`viewset`] for the JSON/API side. See [`template_views`] for
/// usage and the canonical Tera context shape.
///
/// v0.38 slice 22 — fully tri-dialect. State carries Pool enum;
/// handlers route through select_rows_as_json /
/// select_one_row_as_json / insert_returning_pool /
/// update_pool / delete_pool. PG schema-mode tenants get
/// `SET search_path` via Tenant<Postgres>; sqlite/mysql tenants use
/// database-mode via Tenant<DB>.pool(). MySQL note:
/// success_url interpolation can only resolve `{pk}` placeholders
/// on the MySQL backend (no RETURNING).
#[cfg(feature = "template_views")]
pub mod template_views;

/// Django-shape DEBUG template-error overlay — see [`template_debug`].
/// Issue #386.
pub mod template_debug;

/// Django-shape view shortcuts — `get_object_or_404` / `get_list_or_404`
/// / `render` / `redirect`. See [`shortcuts`]. Issue #10.
#[cfg(feature = "template_views")]
pub mod shortcuts;

/// Django `humanize` template filters — `intcomma`, `intword`,
/// `naturalsize`, `ordinal`, `apnumber`, `naturaltime`, `naturalday`.
/// See [`humanize`]. Issue #17.
#[cfg(feature = "template_views")]
pub mod humanize;

/// Django `defaultfilters` template filters — `pluralize`,
/// `truncatewords`, `linebreaks`, `default_if_none`. See
/// [`default_filters`]. Issue #61.
#[cfg(feature = "template_views")]
pub mod default_filters;

/// Django-shape standalone validators — `validate_email`,
/// `validate_url`, `validate_slug`, `validate_min_length` /
/// `validate_max_length`, `validate_min_value` / `validate_max_value`.
/// See [`validators`]. Issue #54.
pub mod validators;

/// Custom Manager / QuerySet-extension pattern — Django's
/// `class PublishedManager(Manager)` / `QuerySet.as_manager()`
/// adapted to Rust's extension-trait idiom. See [`manager`] for
/// the canonical worked examples. Issue #52.
pub mod manager;

/// Model-inheritance patterns — Django's `Meta.abstract`,
/// multi-table, and proxy shapes adapted to Rust idioms. See
/// [`inheritance`] for the canonical worked mappings. Issue #51.
pub mod inheritance;

/// Composite-primary-key patterns — Django 5.2's `CompositePrimaryKey`
/// mapped to `Auto<i64>` surrogate + `unique_together`. See
/// [`composite_pk`] for the canonical worked pattern. Issue #46.
pub mod composite_pk;

/// Named URL reversal — Django's `reverse(name, params)` +
/// `get_absolute_url()`. Pair with [`register_url!`]. Issue #8.
pub mod urls;

/// Django messages framework — `messages.success/info/warning/error/debug`
/// flash storage backed by a signed cookie. Issue #9.
pub mod messages;

/// Django-shape access decorators — `login_required` middleware +
/// `?next=` round-trip helpers. Issue #11.
pub mod auth_decorators;

/// Pluggable `AUTHENTICATION_BACKENDS` chain — register an ordered
/// list of [`auth_backends::AuthBackend`] impls; the chain returns
/// the first non-`None` result. Ships `RemoteUserBackend` for SSO
/// proxy deployments. Issue #54 (partial).
pub mod auth_backends;

/// Pluggable `AUTH_PASSWORD_VALIDATORS` chain — run an ordered list
/// of [`password_validators::PasswordValidator`]s at signup /
/// password-change. Built-ins: `MinimumLengthValidator`,
/// `MaximumLengthValidator`, `NumericPasswordValidator`,
/// `UserAttributeSimilarityValidator`, `CommonPasswordValidator`.
/// Issue #54 (partial).
pub mod password_validators;

/// Pluggable `PASSWORD_HASHERS` chain with **upgrade-on-login** —
/// register an ordered list of [`password_hashers::PasswordHasher`]s;
/// the first is preferred for new hashes, every entry verifies its
/// own format, and successful logins against an older hasher return
/// a freshly-rehashed value so the caller can transparently
/// upgrade the stored hash. Issue #54 (partial).
pub mod password_hashers;

/// Signed-cookie session primitives — HMAC-SHA256 key wrapper +
/// `sign(secret, msg)` helper. Shared by every layer that ships a
/// signed cookie ([`tenancy::session`], [`admin::session`]) so the
/// crypto lives in one place. See [`session::SessionSecret`].
///
/// Compiled when either the `admin` or `tenancy` feature is on —
/// both bring in the underlying HMAC + base64 crates as transitive
/// deps. Bare-ORM builds (`default-features = false` without admin
/// / tenancy) skip the module entirely.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod session;

/// TTY-gated interactive prompts for `manage` verbs — `ask(prompt)`
/// reads a line, `ask_password(prompt)` reads a password without
/// echoing. Both return `Ok(None)` on non-TTY stdin so scripted
/// callers keep the existing "missing required flag" error path.
///
/// Shared by `manage create-admin` (bare admin) and
/// `manage create-operator` / `create-user` (tenancy). Promoted to
/// the crate root in v0.45 (#253 slice B); pre-v0.45 it lived
/// under `tenancy::manage_interactive`.
#[cfg(any(feature = "admin", feature = "tenancy"))]
pub mod manage_interactive;

/// XML sitemap rendering — `django.contrib.sitemaps`. Build a
/// `Sitemap` impl (or pass a `Vec<SitemapEntry>` directly) and call
/// [`sitemaps::render_sitemap`]; for large sites use
/// [`sitemaps::render_sitemap_index`] to point crawlers at multiple
/// child sitemaps. Issue #57.
pub mod sitemaps;

/// RSS 2.0 + Atom 1.0 feed rendering —
/// `django.contrib.syndication`. Build a [`syndication::Feed`] with
/// channel metadata + [`syndication::FeedItem`]s and call
/// [`syndication::render_rss`] or [`syndication::render_atom`].
/// Issue #57.
pub mod syndication;

/// Table-driven HTTP redirects — `django.contrib.redirects`. Build a
/// [`redirects::RedirectMap`] (programmatically or from a CSV) and
/// mount [`redirects::redirects_middleware`] on your axum router;
/// matching requests short-circuit with a 301/302 response and the
/// canonical URL in `Location`, query strings preserved. Issue #57.
pub mod redirects;

/// Static "flat pages" — `django.contrib.flatpages`. Build a
/// [`flatpages::FlatPageMap`] (path → [`flatpages::FlatPage`]) and
/// mount [`flatpages::flatpages_middleware`] on your axum router;
/// matching requests serve the page body with `text/html` (or a
/// custom content-type). Bring your own Tera wrapping. Issue #57.
pub mod flatpages;

/// Django-shape test assertion helpers — `assert_contains` /
/// `assert_redirects` / `assert_status` / `assert_messages` on axum
/// Response objects. Issue #40.
pub mod test_assertions;

/// Tag-based test filtering — Django's `@tag('slow')` +
/// `manage test --tag fast --exclude-tag slow`. Drop `tags!("slow")`
/// at the top of a `#[test]` body; the test self-skips when the
/// `RUSTANGO_TEST_TAGS` / `RUSTANGO_TEST_EXCLUDE_TAGS` env vars
/// filter it out. Issue #45.
pub mod test_filter;

/// Class-level test fixtures — Django's `setUpTestData(cls)`. Wraps
/// `std::sync::OnceLock` / `tokio::sync::OnceCell` with the
/// [`setup_test_data!`] / [`setup_test_data_async!`] macros so test
/// fixtures lazily init once per test-binary process. Issue #42.
pub mod test_data;

/// Django-shape test factories — `factory_boy` parity. [`test_factory::Sequence`]
/// counter + [`test_factory::Factory`] trait with `build_batch`
/// for declarative model builders. Issue #432.
pub mod test_factory;

/// Test-only Settings overlay — Django's `@override_settings` /
/// `with self.settings(...)`. Wraps a `tokio::task_local!`
/// [`config::Settings`] overlay so test code can install per-task
/// configuration without touching process-global state.
/// [`test_settings::current`] reads through the overlay → fallback
/// chain. Issue #43.
#[cfg(feature = "config")]
pub mod test_settings;

/// Test-time DB isolation — Django's `TestCase` transaction
/// wrapping. [`test_db::with_rollback`] runs an async closure
/// inside a transaction that always rolls back, so test mutations
/// don't leak into the next test. Issue #39 partial.
pub mod test_db;

/// `manage dbshell` — spawn `psql` / `mysql` / `sqlite3` for the
/// current `DATABASE_URL`. Issue #56 (partial).
pub mod dbshell;

/// Django-style runserver — [`server::Builder`] owns every line of
/// boilerplate every tenancy app would otherwise rewrite (DB pool,
/// resolver chain, host dispatch, operator console, bind + serve).
///
/// Module itself is unconditional so the bi-dialect single-pool
/// [`server::AppBuilder`] is available without the `tenancy` feature.
/// The full multi-tenant [`server::Builder`] is gated inside the
/// module on `tenancy`.
pub mod server;

/// Unified manage runner — collapses `src/main.rs` + `src/bin/manage.rs`
/// boilerplate into one [`manage::Cli`] builder. Tenancy variant
/// available when the `tenancy` feature is on. Behind the `manage`
/// feature (default-on) which pulls a minimal axum + tokio surface
/// so bare-API projects (no admin) can still use the dispatcher.
#[cfg(feature = "manage")]
pub mod manage;

/// `#[rustango::main]` — the Django-shape `runserver` entrypoint.
/// Wraps `#[tokio::main]` with a default `tracing-subscriber` boot
/// (env-filter, falling back to `info,sqlx=warn`). Available behind
/// the `runtime` feature, which `tenancy` implies.
#[cfg(feature = "runtime")]
pub use rustango_macros::main;

/// Internal re-exports for proc-macros that need to name third-party
/// crates without forcing the user to add them to their `Cargo.toml`.
/// Not part of the public API — names here may change between minors.
#[doc(hidden)]
#[cfg(feature = "runtime")]
pub mod __private_runtime {
    /// v0.31.1 (#4) — re-exported so `#[rustango::main]` can resolve
    /// `tokio::main` through the rustango facade. Apps using the
    /// shim no longer need a direct `tokio` dependency.
    pub use tokio;
    pub use tracing_subscriber;
}

/// Proc-macros crate, re-exported. End users normally reach
/// [`Model`] and [`embed_migrations`] directly via the facade rather
/// than naming `macros`.
pub use rustango_macros as macros;

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
///     .fetch_pool(&pool).await?;
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
/// annotation. Re-exported only when the `tenancy` feature is on.
#[cfg(feature = "tenancy")]
pub use rustango_macros::ViewSet;

/// `#[derive(Serializer)]` — implements [`serializer::ModelSerializer`] on a
/// struct, generating `from_model`, a custom `serde::Serialize` (respecting
/// `write_only`), and `writable_fields`. Re-exported when the `serializer`
/// feature is on.
#[cfg(feature = "serializer")]
pub use rustango_macros::Serializer;
