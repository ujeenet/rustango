//! rustango — a Django-inspired ORM + admin + multi-tenancy for Rust.
//!
//! ```ignore
//! [dependencies]
//! rustango = { version = "0.7", features = ["tenancy"] }
//! ```
//!
//! Out of the box (`default = ["postgres", "admin"]`) you get the
//! ORM, the migration runner, and the auto-admin. Add `"tenancy"`
//! for the multi-tenant resolver / pools / per-tenant auth pieces.
//! Drop `default-features` for the bare ORM (no axum, no Tera).
//!
//! See the workspace [README](https://github.com/ujeenet/rustango)
//! for the full feature matrix and the Django-shape project layout.

// Lets `::rustango::core::Model` (emitted by the proc-macro) and
// `rustango::sql::Auto<i64>` (used in tenancy source code carried
// over from the pre-collapse layout) resolve to ourselves without
// rewriting either.
extern crate self as rustango;

pub mod audit;
pub mod core;
pub mod migrate;
pub mod query;
pub mod sql;

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

/// Django-shape model signals — [`signals::connect_post_save`] etc.
/// Receivers register globally per model type and run sequentially.
#[cfg(feature = "signals")]
pub mod signals;

/// CORS middleware — [`cors::CorsLayer`] for axum routers.
#[cfg(feature = "admin")]
pub mod cors;

/// Token-bucket rate limiting middleware — [`rate_limit::RateLimitLayer`].
/// Per-IP, per-header, or global. Returns 429 with `Retry-After` when exhausted.
#[cfg(feature = "admin")]
pub mod rate_limit;

/// Health check endpoints — `/health` (liveness) + `/ready` (readiness).
/// See [`health::health_router`].
#[cfg(feature = "admin")]
pub mod health;

/// Email backends — [`email::Mailer`] trait + console/in-memory/null backends.
#[cfg(feature = "email")]
pub mod email;

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
#[cfg(feature = "tenancy")]
pub mod bulk_actions;

/// TOTP — RFC 6238 time-based one-time passwords for 2FA.
/// See [`totp::generate`] / [`totp::verify`] / [`totp::otpauth_url`].
#[cfg(feature = "totp")]
pub mod totp;

/// Text utilities — slugify, html_escape, truncate.
pub mod text;

/// Request ID middleware — assign per-request correlation IDs.
/// See [`request_id::RequestIdLayer`].
#[cfg(all(feature = "admin", feature = "tenancy"))]
pub mod request_id;

/// IP allowlist / blocklist middleware. See [`ip_filter::IpFilterLayer`].
#[cfg(feature = "admin")]
pub mod ip_filter;

/// Webhook signature verification (HMAC-SHA256). See [`webhook::verify_signature`].
#[cfg(feature = "webhook")]
pub mod webhook;

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

/// OAuth2 / OIDC swiss-knife — social login that works for both pure
/// OAuth2 (GitHub, Discord) and OIDC (Google, Microsoft, Keycloak)
/// providers via the `/userinfo` endpoint. Per-tenant via
/// [`oauth2::OAuth2Registry`]. Optional axum router under [`oauth2::router`]
/// (requires the `admin` feature).
#[cfg(feature = "oauth2")]
pub mod oauth2;

#[cfg(feature = "tenancy")]
pub mod tenancy;

/// Per-request extractors for handlers — tenancy-aware DI. Today
/// ships [`extractors::Tenant`]; future slices add `Operator` + `User`.
#[cfg(feature = "tenancy")]
pub mod extractors;

/// DRF-style ModelViewSet — five REST endpoints for any [`Model`] table
/// in ~5 lines. See [`viewset::ViewSet`].
#[cfg(feature = "tenancy")]
pub mod viewset;

/// Django-style runserver — [`server::Builder`] owns every line of
/// boilerplate every tenancy app would otherwise rewrite (DB pool,
/// resolver chain, host dispatch, operator console, bind + serve).
#[cfg(feature = "tenancy")]
pub mod server;

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

/// Server-assigned PK wrapper. `id: Auto<i64>` → `BIGSERIAL`. See
/// [`sql::Auto`] for details.
pub use sql::Auto;

/// Bake every migration file in a directory into the binary at
/// compile time, for shipping a single-binary distribution. Pair
/// with [`migrate::migrate_embedded`].
pub use rustango_macros::embed_migrations;

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
