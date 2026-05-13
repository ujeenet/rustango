//! Unified manage runner — collapses `src/main.rs` + `src/bin/manage.rs`
//! boilerplate into one builder so apps stop hand-writing the
//! dispatcher.
//!
//! ```ignore
//! mod apps;
//! mod settings;
//!
//! #[rustango::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     rustango::manage::Cli::new()
//!         .api(apps::api())
//!         .seed(apps::seed)
//!         .run().await
//! }
//! ```
//!
//! `Cli::run()` reads `std::env::args()` and dispatches:
//!
//! * (no args) or `runserver` — open the pool from `DATABASE_URL`,
//!   apply pending migrations, mount the user's API router, serve.
//! * everything else — forward to [`crate::migrate::manage::run`]
//!   (or [`crate::tenancy::manage::run`] when [`Cli::tenancy`] is on).
//!
//! The dispatcher owns the `cargo run` vs `cargo run -- migrate` split
//! so users have one binary instead of two.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use axum::Router;

// v0.38 — `PgPool` only used in the non-tenancy `runserver` path
// (which stays PG-only by signature until v0.39); gated accordingly.
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;

/// Boxed seed-hook future. Keeps the public method signature simple
/// while accepting any `async fn(&Pool) -> Result<…>` closure.
type SeedFut<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error>>> + Send + 'a>>;
type SeedFn = Box<dyn for<'a> FnOnce(&'a crate::sql::Pool) -> SeedFut<'a> + Send>;

/// One-builder dispatcher. Hand it your API router (and optionally a
/// seed hook), call [`Cli::run`], and you're done.
#[must_use = "Cli does nothing until .run() is awaited"]
pub struct Cli {
    api: Router,
    seed: Option<SeedFn>,
    bind: String,
    migrations_dir: PathBuf,
    tenancy: bool,
    /// Optional override for the framework's reserved URL prefixes
    /// (`/__login`, `/__admin`, `/__audit`, …). Plumbed through to
    /// [`crate::server::Builder::routes`] when [`Cli::tenancy`] is on.
    /// `None` keeps the v0.27 defaults.
    #[cfg(feature = "tenancy")]
    routes: Option<crate::tenancy::RouteConfig>,
    /// Bootstrap initializer used by the `init-tenancy` verb when
    /// [`Cli::tenancy`] is on. Defaults to
    /// [`crate::tenancy::init_tenancy`]; replaced by [`Cli::user_model`]
    /// to swap in a custom [`crate::tenancy::TenantUserModel`].
    #[cfg(all(feature = "tenancy", feature = "postgres"))]
    init_tenancy_fn: crate::tenancy::manage::InitTenancyFn,
    /// Cloned [`Settings`] handle stored by [`Cli::with_settings`].
    /// Consumed at `runserver` time to apply layers (security_headers,
    /// CORS, access_log, body_limit) on top of the user's API
    /// router so a single `with_settings_from_env()` call drives
    /// the whole stack.
    #[cfg(feature = "config")]
    settings_for_layers: Option<crate::config::Settings>,
    /// When `true`, mounts `/health` + `/ready` endpoints on the
    /// API router at runserver time. Set via [`Cli::with_health`].
    /// Default `false` because operators sometimes want their own
    /// health endpoint shape (custom JSON, additional checks).
    health_endpoints: bool,
    /// `(prefix, root_dir)` pairs registered via [`Cli::with_static`].
    /// Mounted at `runserver` time as
    /// `Router::nest(prefix, static_router(StaticFiles::new(root_dir)))`.
    /// Empty by default — projects that already mount their own
    /// `static_files::static_router` keep doing it.
    #[cfg(feature = "admin")]
    static_dirs: Vec<(String, PathBuf)>,
    /// CSRF middleware config registered via [`Cli::with_csrf`]. `None`
    /// means no CSRF layer mounted — the right default for pure JSON
    /// APIs that authenticate via JWT and reject form-encoded bodies
    /// at the deserializer layer. Form-driven apps (anything using
    /// `template_views` Create/Update/DeleteView) opt in.
    #[cfg(feature = "csrf")]
    csrf: Option<crate::forms::csrf::CsrfConfig>,
    /// When `true`, mounts [`crate::welcome::welcome_router`] at `/`
    /// at runserver time. Default off — projects that already have a
    /// root handler (or want a 404 on `/`) shouldn't have their route
    /// table silently rewritten by the framework. Set via
    /// [`Cli::with_welcome`].
    welcome_page: bool,
    /// When `true`, install `tracing-subscriber` from the loaded
    /// `Settings.logging` section before serving (roadmap #8,
    /// v0.30.11). Default off so existing projects that call
    /// `rustango::logging::setup()` themselves don't get a double
    /// init. The returned `WorkerGuard` (when a file sink is
    /// configured) is stashed on `Cli` for the lifetime of the
    /// runserver future.
    #[cfg(all(feature = "config", feature = "runtime"))]
    install_logging: bool,
}

impl Cli {
    /// Default builder — empty router, no seed, binds `0.0.0.0:8080`,
    /// migrations live in `./migrations`, single-tenant.
    #[must_use]
    pub fn new() -> Self {
        Self {
            api: Router::new(),
            seed: None,
            bind: std::env::var("RUSTANGO_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            migrations_dir: PathBuf::from("./migrations"),
            tenancy: false,
            #[cfg(feature = "tenancy")]
            routes: None,
            #[cfg(all(feature = "tenancy", feature = "postgres"))]
            init_tenancy_fn: crate::tenancy::init_tenancy,
            #[cfg(feature = "config")]
            settings_for_layers: None,
            health_endpoints: false,
            #[cfg(feature = "admin")]
            static_dirs: Vec::new(),
            #[cfg(feature = "csrf")]
            csrf: None,
            welcome_page: false,
            #[cfg(all(feature = "config", feature = "runtime"))]
            install_logging: false,
        }
    }

    /// Override the framework's reserved URL prefixes. Equivalent to
    /// calling [`crate::server::Builder::routes`] directly when
    /// constructing the server outside of [`Cli`]. No-op when
    /// [`Cli::tenancy`] is not enabled (single-tenant projects don't
    /// have these reserved paths to begin with).
    ///
    /// ```ignore
    /// use rustango::tenancy::RouteConfig;
    ///
    /// rustango::manage::Cli::new()
    ///     .tenancy()
    ///     .routes(RouteConfig::friendly())   // /login, /admin, /audit
    ///     .api(urls::api())
    ///     .run().await
    /// ```
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn routes(mut self, routes: crate::tenancy::RouteConfig) -> Self {
        self.routes = Some(routes);
        self
    }

    /// Mount the user's stateless API router. Pool is injected via
    /// `axum::Extension<PgPool>` at serve time so handlers can pull
    /// it without managing state themselves.
    #[must_use]
    pub fn api(mut self, router: Router) -> Self {
        self.api = router;
        self
    }

    /// Run a one-shot async hook on first boot — typical use is
    /// inserting a demo tenant or a seed superuser. The hook receives
    /// the registry pool (or single-tenant pool when [`Cli::tenancy`]
    /// is off) as a tri-dialect [`crate::sql::Pool`].
    #[must_use]
    pub fn seed<F, Fut>(mut self, hook: F) -> Self
    where
        F: for<'a> FnOnce(&'a crate::sql::Pool) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send + 'static,
    {
        self.seed = Some(Box::new(move |pool| Box::pin(hook(pool))));
        self
    }

    /// Override the bind address. Defaults to `RUSTANGO_BIND` env or
    /// `0.0.0.0:8080`.
    #[must_use]
    pub fn bind(mut self, addr: impl Into<String>) -> Self {
        self.bind = addr.into();
        self
    }

    /// Auto-mount `/health` (liveness) and `/ready` (readiness +
    /// `SELECT 1`) endpoints on the API router at `runserver` time.
    /// Default off — operators sometimes ship custom health JSON
    /// or layer additional checks (Redis ping, queue depth, etc.)
    /// and don't want the framework's defaults colliding.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(urls::api())
    ///     .with_settings_from_env()
    ///     .with_health()
    ///     .run().await
    /// ```
    ///
    /// The mounted endpoints come from
    /// [`crate::health::health_router`] — `/health` always 200s,
    /// `/ready` 200s when the database is reachable and 503s
    /// otherwise. Production deployments wire the load balancer
    /// to `/ready` for traffic gating and `/health` for liveness
    /// probes.
    #[must_use]
    pub fn with_health(mut self) -> Self {
        self.health_endpoints = true;
        self
    }

    /// Auto-mount a [`crate::static_files::static_router`] at `prefix`
    /// serving files under `root_dir`. Repeat the call to mount more
    /// than one directory (e.g. `/static` from `./assets`,
    /// `/uploads` from `./var/uploads`).
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(urls::api())
    ///     .with_static("/static", "./assets")
    ///     .with_static("/uploads", "./var/uploads")
    ///     .run().await
    /// ```
    ///
    /// Defaults from [`crate::static_files::StaticFiles::new`] —
    /// `Cache-Control: public, max-age=3600`, dotfiles 404, symlink
    /// escapes blocked. Projects that need finer control (immutable
    /// hash-named bundles, `.well-known` whitelisting) keep mounting
    /// `static_router` directly on their own router and skip this
    /// shortcut. Mount order is preserved — first registered prefix
    /// is checked first when paths overlap.
    #[cfg(feature = "admin")]
    #[must_use]
    pub fn with_static(mut self, prefix: impl Into<String>, root_dir: impl Into<PathBuf>) -> Self {
        self.static_dirs.push((prefix.into(), root_dir.into()));
        self
    }

    /// Auto-mount the [`crate::forms::csrf::CsrfLayer`] on the API
    /// router at `runserver` time using
    /// [`crate::forms::csrf::CsrfConfig::default`]. Required for any
    /// project using the HTML CBVs (`template_views`'s
    /// `CreateView`/`UpdateView`/`DeleteView`) — those views call
    /// `csrf::ensure_token` to mint the cookie + form value, and the
    /// layer enforces it on POST/PUT/PATCH/DELETE.
    ///
    /// Default off — pure JSON APIs that authenticate via JWT
    /// (`Authorization: Bearer ...`) don't need CSRF and shouldn't
    /// pay the body-buffer cost on form-encoded POSTs that they'll
    /// reject anyway.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(urls::api())
    ///     .with_csrf()                      // form-driven app
    ///     .run().await
    /// ```
    ///
    /// To override the cookie name / `Secure` attribute, use
    /// [`Cli::with_csrf_config`] instead.
    #[cfg(feature = "csrf")]
    #[must_use]
    pub fn with_csrf(mut self) -> Self {
        self.csrf = Some(crate::forms::csrf::CsrfConfig::default());
        self
    }

    /// Same as [`Cli::with_csrf`] but with explicit
    /// [`crate::forms::csrf::CsrfConfig`] — for projects that need a
    /// non-default cookie name (when stacking against another
    /// framework on the same host) or `Secure` flag in production.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(urls::api())
    ///     .with_csrf_config(rustango::forms::csrf::CsrfConfig {
    ///         secure: true,
    ///         ..Default::default()
    ///     })
    ///     .run().await
    /// ```
    #[cfg(feature = "csrf")]
    #[must_use]
    pub fn with_csrf_config(mut self, cfg: crate::forms::csrf::CsrfConfig) -> Self {
        self.csrf = Some(cfg);
        self
    }

    /// Auto-mount [`crate::welcome::welcome_router`] at `/` so a fresh
    /// project boots to a friendly "rustango — it works!" page
    /// instead of an empty-router 404. Default off — projects that
    /// already have a root handler shouldn't have their route table
    /// silently rewritten.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(urls::api())
    ///     .with_welcome()                     // first-run friendliness
    ///     .run().await
    /// ```
    ///
    /// Mounted via `Router::merge`, which means a `/` route inside
    /// `urls::api()` would collide and panic. Drop the call once your
    /// own root handler is wired.
    #[must_use]
    pub fn with_welcome(mut self) -> Self {
        self.welcome_page = true;
        self
    }

    /// Install `tracing-subscriber` from the loaded
    /// [`crate::config::Settings::logging`] section at runserver
    /// time. Equivalent to calling
    /// [`crate::logging::Setup::from_settings`] yourself + holding
    /// onto the returned `WorkerGuard` for the process lifetime —
    /// just removes the boilerplate. Roadmap #8, v0.30.11.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .with_settings_from_env()
    ///     .with_logging()                  // installs from Settings.logging
    ///     .api(urls::api())
    ///     .run().await
    /// ```
    ///
    /// Default off so projects that call `rustango::logging::setup()`
    /// themselves don't get a duplicate init. `tracing-subscriber`'s
    /// `try_init` would silently no-op on the second call anyway,
    /// but the explicit opt-in keeps the surface predictable.
    ///
    /// Call ordering: works regardless of where in the chain it
    /// sits — the install happens at `run()` time, not when
    /// `with_logging` is called. So
    /// `Cli::new().with_logging().with_settings_from_env()` and
    /// `Cli::new().with_settings_from_env().with_logging()` both
    /// install based on the final `Settings.logging` snapshot.
    #[cfg(all(feature = "config", feature = "runtime"))]
    #[must_use]
    pub fn with_logging(mut self) -> Self {
        self.install_logging = true;
        self
    }

    /// Apply values from a loaded `Settings` struct (#87 wiring,
    /// v0.29). Honors:
    ///
    /// - `Settings.server.bind` → bind address. The `RUSTANGO_BIND`
    ///   env var still wins (deploy-time overrides need to beat
    ///   committed config), and any subsequent explicit
    ///   [`Cli::bind`] call wins over both.
    ///
    /// Future fields land here as the wiring catches up — the method
    /// is forward-compatible because every Settings field is
    /// `Option`-typed (a missing key falls through, doesn't reset).
    ///
    /// ```ignore
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// rustango::manage::Cli::new()
    ///     .with_settings(&cfg)
    ///     .api(urls::api())
    ///     .run().await
    /// ```
    #[cfg(feature = "config")]
    #[must_use]
    pub fn with_settings(mut self, s: &crate::config::Settings) -> Self {
        // Resolution priority for bind (most-specific wins):
        //   1. an explicit `.bind(...)` call AFTER this one
        //   2. `RUSTANGO_BIND` env var
        //   3. `Settings.server.bind` (this branch)
        //   4. hardcoded `0.0.0.0:8080` (Cli::new fallback)
        //
        // Env wins over TOML so deploy-time emergency overrides
        // don't require a config push + restart.
        if std::env::var("RUSTANGO_BIND").is_err() {
            if let Some(bind) = s.server.bind.as_deref() {
                self.bind = bind.to_owned();
            }
        }

        // Settings.routes → RouteConfig. Build the right preset
        // (friendly default / legacy v0.28) and apply per-field
        // overrides on top, so the TOML can mix-and-match.
        // Single-tenant builds (no `tenancy` feature) skip this
        // branch — RouteConfig is a tenancy-only construct.
        #[cfg(feature = "tenancy")]
        {
            self.routes = Some(routes_from_settings(&s.routes, self.routes.take()));
        }

        // Stash a clone for `runserver` to apply layered settings
        // (security_headers, CORS, access_log, body_limit) on top
        // of the user's `api` Router. Done at run time rather than
        // here because `.api(...)` may be called either before or
        // after `.with_settings(...)` and we want consistent
        // behavior either way.
        self.settings_for_layers = Some(s.clone());
        self
    }

    /// Convenience: run `Settings::load_from_env()` and apply via
    /// [`Cli::with_settings`]. Equivalent to:
    ///
    /// ```ignore
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// rustango::manage::Cli::new().with_settings(&cfg)
    /// ```
    ///
    /// Returns the original [`Cli`] unchanged when the layered
    /// loader fails (e.g. `config/default.toml` missing) so projects
    /// that haven't adopted the layered loader still build cleanly.
    /// Errors are surfaced via `tracing::warn` so they're visible
    /// without breaking startup.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn with_settings_from_env(self) -> Self {
        match crate::config::Settings::load_from_env() {
            Ok(cfg) => self.with_settings(&cfg),
            Err(e) => {
                tracing::warn!(target: "rustango::manage", error = %e, "Cli::with_settings_from_env: failed to load Settings; falling back to Cli defaults");
                self
            }
        }
    }

    /// Override the migrations directory. Defaults to `./migrations`.
    #[must_use]
    pub fn migrations_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.migrations_dir = dir.into();
        self
    }

    /// Switch dispatch to the multi-tenant code path —
    /// [`crate::tenancy::manage::run`] handles `create-tenant`,
    /// `migrate-tenants`, `create-operator`, `create-user` plus every
    /// single-tenant verb. `runserver` defers to
    /// [`crate::server::Builder`].
    #[must_use]
    pub fn tenancy(mut self) -> Self {
        self.tenancy = true;
        self
    }

    /// Swap the tenant user model used by the `init-tenancy` verb.
    /// Implement [`crate::tenancy::TenantUserModel`] on a model that
    /// declares extra columns on `rustango_users` (display name,
    /// timezone, …) and pass it here — the materialized bootstrap
    /// migration will then `CREATE TABLE` with those extras included.
    ///
    /// Only meaningful in tenancy mode and only on the very first
    /// `init-tenancy`: subsequent invocations are idempotent and
    /// won't rewrite the migration JSON.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(apps::api())
    ///     .tenancy()
    ///     .user_model::<myapp::AppUser>()
    ///     .run().await
    /// ```
    #[cfg(all(feature = "tenancy", feature = "postgres"))]
    #[must_use]
    pub fn user_model<U: crate::tenancy::TenantUserModel>(mut self) -> Self {
        self.init_tenancy_fn = crate::tenancy::init_tenancy_with::<U>;
        self
    }

    /// Read argv, dispatch.
    ///
    /// # Errors
    /// Surfaces whatever the underlying dispatcher / server returns.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        // v0.30.11 — install logging here (the outermost dispatch
        // point) so the WorkerGuard outlives BOTH the runserver
        // future AND the management-verb dispatch path. Installing
        // inside `runserver` would let the guard drop early when
        // `runserver_tenancy` consumes `self`. Holding it here
        // until `run()` returns covers every verb cleanly.
        #[cfg(all(feature = "config", feature = "runtime"))]
        let _logging_guard = if self.install_logging {
            let s = self
                .settings_for_layers
                .as_ref()
                .map(|s| s.logging.clone())
                .unwrap_or_default();
            crate::logging::Setup::from_settings(&s).install()
        } else {
            None
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let verb = args.first().map_or("", String::as_str);

        match verb {
            // v0.31.1 (#1): the hyphenated `run-server` form is what
            // `--help` has advertised since the start, but only the
            // unhyphenated `runserver` reached `runserver()` — the
            // hyphenated form fell through to `dispatch()` →
            // `tenancy::manage::run_with_init`, which **silently
            // skipped the `.seed()` hook**. Accept both forms.
            "" | "runserver" | "run-server" => self.runserver().await,
            _ => self.dispatch(args).await,
        }
    }

    async fn dispatch(self, args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
        // Verbs that print info and never touch the DB. We let these
        // run even when DATABASE_URL is unset so users can scaffold or
        // read help without configuring Postgres first.
        let no_db_verb = matches!(
            args.first().map(String::as_str),
            Some("help")
                | Some("--help")
                | Some("-h")
                | Some("startapp")
                | Some("makemigrations")
                | Some("docs")
                | Some("version")
                | Some("--version")
                | Some("make:viewset")
                | Some("make:serializer")
                | Some("make:form")
                | Some("make:job")
                | Some("make:notification")
                | Some("make:middleware")
                | Some("make:test")
        );
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://offline".into());
        if !no_db_verb && std::env::var("DATABASE_URL").is_err() {
            return Err("missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`.".into());
        }

        #[cfg(all(feature = "tenancy", feature = "postgres"))]
        if self.tenancy {
            // v0.38 — dispatch on the URL scheme so binaries compiled
            // with multiple backend features (e.g. `["postgres", "sqlite"]`
            // for testing) can route sqlite / mysql URLs to the right
            // `TenantPools<DB>`. Schema-mode tenants on non-PG backends
            // still return a clear validation error at request time
            // (`TenantPools<DB>::scoped_pool_dyn`); database-mode
            // tenants work on every backend.
            let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
            #[cfg(feature = "sqlite")]
            if scheme == "sqlite" {
                let pool = if no_db_verb {
                    crate::sql::sqlx::SqlitePool::connect_lazy(&url)?
                } else {
                    crate::sql::sqlx::SqlitePool::connect(&url).await?
                };
                let pools = crate::tenancy::TenantPools::<crate::sql::sqlx::Sqlite>::new(pool);
                crate::tenancy::manage::run_with_init(
                    &pools,
                    &url,
                    &self.migrations_dir,
                    args,
                    self.init_tenancy_fn,
                )
                .await?;
                return Ok(());
            }
            #[cfg(feature = "mysql")]
            if scheme == "mysql" {
                let pool = if no_db_verb {
                    crate::sql::sqlx::MySqlPool::connect_lazy(&url)?
                } else {
                    crate::sql::sqlx::MySqlPool::connect(&url).await?
                };
                let pools = crate::tenancy::TenantPools::<crate::sql::sqlx::MySql>::new(pool);
                crate::tenancy::manage::run_with_init(
                    &pools,
                    &url,
                    &self.migrations_dir,
                    args,
                    self.init_tenancy_fn,
                )
                .await?;
                return Ok(());
            }
            if !matches!(scheme.as_str(), "postgres" | "postgresql") {
                return Err(format!(
                    "manage::Cli::tenancy() got `DATABASE_URL` with unsupported scheme \
                     `{scheme}`. Supported schemes depend on the compiled-in backend \
                     features (`postgres`, `sqlite`, `mysql`). Schema-mode multi-tenancy \
                     (where many tenants share one PG database via `SET search_path`) \
                     is Postgres-only by language; database-mode multi-tenancy works on \
                     every backend."
                )
                .into());
            }
            let pool = if no_db_verb {
                PgPool::connect_lazy(&url)?
            } else {
                PgPool::connect(&url).await?
            };
            let pools = crate::tenancy::TenantPools::new(pool);
            crate::tenancy::manage::run_with_init(
                &pools,
                &url,
                &self.migrations_dir,
                args,
                self.init_tenancy_fn,
            )
            .await?;
            return Ok(());
        }
        #[cfg(all(feature = "tenancy", not(feature = "postgres")))]
        if self.tenancy {
            // v0.38 — on non-PG builds, the tenancy manage CLI runs
            // through `TenantPools<DB>` with sqlite or mysql as the
            // backend. Schema-mode is rejected (PG-only by language)
            // but database-mode tenants are fully supported through
            // the tri-dialect `_pool` family.
            #[cfg(feature = "sqlite")]
            {
                let p = if no_db_verb {
                    crate::sql::sqlx::SqlitePool::connect_lazy(&url)?
                } else {
                    crate::sql::sqlx::SqlitePool::connect(&url).await?
                };
                let pools = crate::tenancy::TenantPools::<crate::sql::sqlx::Sqlite>::new(p);
                let init_fn: crate::tenancy::manage::InitTenancyFn = crate::tenancy::init_tenancy;
                crate::tenancy::manage::run_with_init(
                    &pools,
                    &url,
                    &self.migrations_dir,
                    args,
                    init_fn,
                )
                .await?;
                return Ok(());
            }
            #[cfg(all(not(feature = "sqlite"), feature = "mysql"))]
            {
                let p = if no_db_verb {
                    crate::sql::sqlx::MySqlPool::connect_lazy(&url)?
                } else {
                    crate::sql::sqlx::MySqlPool::connect(&url).await?
                };
                let pools = crate::tenancy::TenantPools::<crate::sql::sqlx::MySql>::new(p);
                let init_fn: crate::tenancy::manage::InitTenancyFn = crate::tenancy::init_tenancy;
                crate::tenancy::manage::run_with_init(
                    &pools,
                    &url,
                    &self.migrations_dir,
                    args,
                    init_fn,
                )
                .await?;
                return Ok(());
            }
            #[cfg(not(any(feature = "sqlite", feature = "mysql")))]
            {
                let _ = (url, args);
                return Err(
                    "Cli::tenancy() requires at least one of `postgres`, `sqlite`, or \
                     `mysql` features to be enabled."
                        .into(),
                );
            }
        }
        #[cfg(not(feature = "tenancy"))]
        if self.tenancy {
            return Err("Cli::tenancy() requires the `tenancy` feature".into());
        }

        // v0.38 — `migrate::manage::run` accepts `&Pool` now, so the
        // non-tenancy dispatch path opens through `Pool::connect()`
        // which routes by URL scheme to PG / MySQL / SQLite. This is
        // what makes `cargo run -- migrate` work against a sqlite
        // DATABASE_URL without going through the `runserver_tenancy`
        // path.
        let pool = if no_db_verb {
            crate::sql::Pool::connect_lazy(&url)
                .map_err(|e| format!("connect_lazy({url}): {e}").into())
                as Result<_, Box<dyn std::error::Error>>
        } else {
            crate::sql::Pool::connect(&url)
                .await
                .map_err(|e| format!("connect({url}): {e}").into())
                as Result<_, Box<dyn std::error::Error>>
        }?;
        crate::migrate::manage::run(&pool, &self.migrations_dir, args).await?;
        Ok(())
    }

    async fn runserver(self) -> Result<(), Box<dyn std::error::Error>> {
        // Logging install lives in `run()` (the outermost dispatch
        // point) so the WorkerGuard outlives every runserver +
        // management-verb path uniformly.
        #[cfg(feature = "tenancy")]
        if self.tenancy {
            return self.runserver_tenancy().await;
        }
        #[cfg(not(feature = "tenancy"))]
        if self.tenancy {
            return Err("Cli::tenancy() requires the `tenancy` feature".into());
        }
        // Non-tenancy single-tenant `runserver` on non-PG.
        // Opens a `Pool` enum via Pool::connect (dispatches on URL
        // scheme: sqlite:// → SqlitePool, mysql:// → MySqlPool). Runs
        // migrations through migrate_pool (tri-dialect).
        #[cfg(not(feature = "postgres"))]
        {
            let url = std::env::var("DATABASE_URL").map_err(|_| {
                "missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`."
            })?;
            let pool = crate::sql::Pool::connect(&url).await?;
            let _ = crate::migrate::migrate_pool(&pool, &self.migrations_dir).await?;
            if let Some(seed) = self.seed {
                seed(&pool).await?;
            }
            let api = self.api;
            #[cfg(feature = "admin")]
            let api = if self.welcome_page {
                try_mount_welcome(api)
            } else {
                api
            };
            #[cfg(feature = "admin")]
            let api = mount_static_dirs(api, &self.static_dirs);
            #[cfg(feature = "csrf")]
            let api = match self.csrf {
                Some(cfg) => api.layer(crate::forms::csrf::with_config(cfg)),
                None => api,
            };
            #[cfg(feature = "config")]
            let api = match self.settings_for_layers.as_ref() {
                Some(s) => apply_settings_layers(api, s),
                None => api,
            };
            let app = api.layer(axum::Extension(pool));
            let listener = tokio::net::TcpListener::bind(&self.bind).await?;
            eprintln!("server listening on http://{}", listener.local_addr()?);
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await?;
            return Ok(());
        }
        #[cfg(feature = "postgres")]
        {
            let url = std::env::var("DATABASE_URL").map_err(|_| {
            "missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`."
        })?;
            let pool = PgPool::connect(&url).await?;
            let _ = crate::migrate::migrate(&pool, &self.migrations_dir).await?;
            if let Some(seed) = self.seed {
                seed(&crate::sql::Pool::from(pool.clone())).await?;
            }
            let api = self.api;
            #[cfg(feature = "admin")]
            let api = if self.welcome_page {
                try_mount_welcome(api)
            } else {
                api
            };
            let api = if self.health_endpoints {
                api.merge(crate::health::health_router(pool.clone()))
            } else {
                api
            };
            #[cfg(feature = "admin")]
            let api = mount_static_dirs(api, &self.static_dirs);
            #[cfg(feature = "csrf")]
            let api = match self.csrf {
                Some(cfg) => api.layer(crate::forms::csrf::with_config(cfg)),
                None => api,
            };
            #[cfg(feature = "config")]
            let api = match self.settings_for_layers.as_ref() {
                Some(s) => apply_settings_layers(api, s),
                None => api,
            };
            let app = api.layer(axum::Extension(pool));
            let listener = tokio::net::TcpListener::bind(&self.bind).await?;
            eprintln!("server listening on http://{}", listener.local_addr()?);
            // v0.30.16 — `into_make_service_with_connect_info` is what
            // populates `ConnectInfo<SocketAddr>` in request extensions.
            // Without it, `access_log` (and any other middleware that
            // reads the peer address) sees "-".
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await?;
            Ok(())
        } // end of #[cfg(feature = "postgres")] block for non-tenancy runserver
    }

    // v0.38 — `server::Builder` is the multi-tenant PG runserver
    // (TenantPools + schema-mode dispatch + operator console). Gated
    // to PG today; sqlite/mysql apps with `tenancy` get a friendly
    // runtime error pointing at `DatabasePools<DB>` + plain
    // `axum::serve`. The non-PG runserver_tenancy fallback below
    // mirrors that for symmetry.
    #[cfg(all(feature = "tenancy", feature = "postgres"))]
    async fn runserver_tenancy(self) -> Result<(), Box<dyn std::error::Error>> {
        let api = self.api;
        #[cfg(feature = "admin")]
        let api = if self.welcome_page {
            try_mount_welcome(api)
        } else {
            api
        };
        #[cfg(feature = "csrf")]
        let api = match self.csrf.clone() {
            Some(cfg) => api.layer(crate::forms::csrf::with_config(cfg)),
            None => api,
        };
        #[cfg(feature = "config")]
        let api = match self.settings_for_layers.as_ref() {
            Some(s) => apply_settings_layers(api, s),
            None => api,
        };
        let mut builder = crate::server::Builder::from_env().await?.api(api);
        if self.health_endpoints {
            builder = builder.with_health();
        }
        for (prefix, root) in self.static_dirs {
            builder = builder.with_static(prefix, root);
        }
        if let Some(routes) = self.routes {
            builder = builder.routes(routes);
        }
        if let Some(seed) = self.seed {
            // Tenancy Builder's seed_with takes (Arc<TenantPools>, PgPool,
            // String); we forward the registry pool and discard the rest.
            builder = builder
                .seed_with(move |_pools, registry, _url| async move {
                    seed(&crate::sql::Pool::from(registry)).await
                })
                .await?;
        }
        builder.serve(&self.bind).await
    }

    #[cfg(all(feature = "tenancy", not(feature = "postgres")))]
    async fn runserver_tenancy(self) -> Result<(), Box<dyn std::error::Error>> {
        // v0.38 slice 24 — `server::Builder<DB>` is generic. On non-PG
        // single-backend builds, `DefaultTenantDb` resolves to whichever
        // sqlx backend the feature flags picked (sqlite first, mysql
        // otherwise). Database-mode tenants work out of the box;
        // schema-mode tenants return `TenancyError::Validation` at
        // request time (schema-mode is PG-only by language).
        let api = self.api;
        #[cfg(feature = "admin")]
        let api = if self.welcome_page {
            try_mount_welcome(api)
        } else {
            api
        };
        #[cfg(feature = "csrf")]
        let api = match self.csrf.clone() {
            Some(cfg) => api.layer(crate::forms::csrf::with_config(cfg)),
            None => api,
        };
        #[cfg(feature = "config")]
        let api = match self.settings_for_layers.as_ref() {
            Some(s) => apply_settings_layers(api, s),
            None => api,
        };
        let apex = std::env::var("RUSTANGO_APEX_DOMAIN").unwrap_or_else(|_| "localhost".into());
        let registry_url =
            std::env::var("DATABASE_URL")
                .ok()
                .ok_or_else(|| -> Box<dyn std::error::Error> {
                    "DATABASE_URL not set — sqlite/mysql tenancy projects need an explicit \
             registry URL (e.g. `sqlite:./var/registry.db` or `mysql://…`). Set the \
             env var or build the server manually via `server::Builder::from_pool`."
                        .into()
                })?;
        let registry =
            sqlx::Pool::<crate::tenancy::DefaultTenantDb>::connect(&registry_url).await?;
        let mut builder = crate::server::Builder::<crate::tenancy::DefaultTenantDb>::from_pool(
            registry,
            registry_url,
            apex,
        )
        .api(api);
        if self.health_endpoints {
            builder = builder.with_health();
        }
        for (prefix, root) in self.static_dirs {
            builder = builder.with_static(prefix, root);
        }
        if let Some(routes) = self.routes {
            builder = builder.routes(routes);
        }
        if let Some(seed) = self.seed {
            // Mirror the PG arm above (line 828) so sqlite/mysql tenancy
            // projects get their `Cli::seed` hook fired on boot too.
            builder = builder
                .seed_with(move |_pools, registry, _url| async move {
                    seed(&crate::sql::Pool::from(registry)).await
                })
                .await?;
        }
        builder.serve(&self.bind).await
    }
}

impl Default for Cli {
    fn default() -> Self {
        Self::new()
    }
}

/// Nest each (prefix, root_dir) pair from [`Cli::with_static`] into
/// the API router. Pure function so the runserver path stays linear
/// and unit tests can assert on the post-mount Router without
/// spinning up a TCP listener.
/// Try to merge `welcome_router()` into the user's API router.
/// `Router::merge` panics when both sides claim the same route
/// (the documented v0.29.12 footgun a tenancy project hit during
/// the v0.30.x exercise: tango's `urls::api()` already routed
/// `GET /` for a per-tenant index handler, and merging welcome's
/// `GET /` triggered axum's "Overlapping method route" panic).
///
/// v0.30.15 — wrap the merge in `catch_unwind` so the conflict
/// surfaces as a `tracing::warn!` instead of a process abort.
/// `Router` implements `UnwindSafe` so the catch is sound; the
/// fallback returns the original router unchanged.
#[cfg(feature = "admin")]
fn try_mount_welcome(api: Router) -> Router {
    let api_for_probe = api.clone();
    // v0.37 (#5) — axum's `Router::merge` panics with "Overlapping
    // method route" when both sides route `GET /`. We use
    // `catch_unwind` to convert that panic into a friendly tracing
    // warn. The default Rust panic hook still prints the panic line
    // to stderr though, which is confusing for users — looks like a
    // crash. Install a noop hook for the merge probe so the stderr
    // stays clean; restore the previous hook immediately after.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // suppress
    let merged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        api_for_probe.merge(crate::welcome::welcome_router())
    }));
    std::panic::set_hook(prev_hook);
    match merged {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(
                target: "rustango::manage",
                "Cli::with_welcome() skipped: the API router already routes GET / \
                 (axum: \"Overlapping method route\"). Drop the .with_welcome() call \
                 once you wire your own root handler to silence this warning."
            );
            api
        }
    }
}

#[cfg(feature = "admin")]
fn mount_static_dirs(api: Router, dirs: &[(String, PathBuf)]) -> Router {
    let mut r = api;
    for (prefix, root) in dirs {
        r = r.nest(
            prefix,
            crate::static_files::static_router(crate::static_files::StaticFiles::new(root.clone())),
        );
    }
    r
}

/// Apply security_headers + CORS + access_log + body_limit layers
/// derived from a loaded [`crate::config::Settings`] handle to the
/// user's API router (#87 wiring). Called from `runserver` /
/// `runserver_tenancy` when the user threaded settings via
/// [`Cli::with_settings`] / [`Cli::with_settings_from_env`].
///
/// Layer order (innermost → outermost), matching the canonical
/// recommendation in the README's Production checklist:
///
///   request → access_log → body_limit → CORS → security_headers → handler
///
/// `from_settings` constructors decide whether each layer mounts
/// at all — most return `None` (or are no-ops) when the section
/// has nothing configured, so `with_settings` on a near-empty
/// `default.toml` doesn't surprise the user with unexpected
/// middleware.
#[cfg(feature = "config")]
fn apply_settings_layers(api: Router, s: &crate::config::Settings) -> Router {
    use crate::access_log::{AccessLogLayer, AccessLogRouterExt as _};
    use crate::body_limit::{BodyLimitLayer, BodyLimitRouterExt as _};
    use crate::cors::{CorsLayer, CorsRouterExt as _};
    use crate::request_timeout::{RequestTimeoutLayer, RequestTimeoutRouterExt as _};
    use crate::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt as _};

    let mut app = api;

    // request_timeout (innermost — wraps the handler itself so a
    // wedged future doesn't hold downstream layer state hostage).
    // Opt-in: from_settings returns None when request_timeout_secs
    // is unset or zero.
    if let Some(layer) = RequestTimeoutLayer::from_settings(&s.server) {
        app = app.request_timeout(layer);
    }

    // body_limit — gate on declared body size before the handler
    // even starts.
    if let Some(layer) = BodyLimitLayer::from_settings(&s.server) {
        app = app.body_limit(layer);
    }

    // access_log — extends the redact list with project additions
    // from `[audit] redact_query_params`. Defaults are sensible so
    // the layer mounts unconditionally.
    let log_layer = AccessLogLayer::default().with_audit_settings(&s.audit);
    app = app.access_log(log_layer);

    // CORS — opt-in (returns None when no origins configured).
    if let Some(cors) = CorsLayer::from_settings(&s.security) {
        app = app.cors(cors);
    }

    // security_headers (outermost — every response goes through).
    // SecuritySettings::default() produces strict() so this mounts
    // even when the [security] section is missing entirely.
    let sec = SecurityHeadersLayer::from_settings(&s.security);
    app = app.security_headers(sec);

    app
}

/// Build a [`crate::tenancy::RouteConfig`] from a
/// [`crate::config::RoutesSettings`] section. Used by
/// [`Cli::with_settings`] to translate the declarative TOML
/// (`legacy_preset = true` + per-field overrides) into the
/// runtime config.
///
/// Resolution order:
/// 1. Pick the base preset — `legacy()` if `legacy_preset = true`,
///    `default()` (friendly, post-#85) otherwise.
/// 2. If `existing` is supplied (the user already called
///    [`Cli::routes`]), use it as the base instead — explicit
///    code-side calls win over `with_settings`.
/// 3. Apply each per-field override that's `Some(...)`.
///
/// This way TOML-only projects don't need any code wiring;
/// projects that want code-side construction can keep doing it; and
/// hybrid projects can mix (e.g. set the apex via env, override
/// just `admin_url` in TOML).
#[cfg(all(feature = "config", feature = "tenancy"))]
fn routes_from_settings(
    s: &crate::config::RoutesSettings,
    existing: Option<crate::tenancy::RouteConfig>,
) -> crate::tenancy::RouteConfig {
    use crate::tenancy::RouteConfig;
    let mut rc = if let Some(rc) = existing {
        // Explicit `.routes(...)` call already happened; honor it
        // as the base + just layer per-field TOML overrides.
        rc
    } else if matches!(s.legacy_preset, Some(true)) {
        RouteConfig::legacy()
    } else {
        RouteConfig::default()
    };
    if let Some(v) = s.login_url.as_deref() {
        rc.login_url = v.to_owned();
    }
    if let Some(v) = s.logout_url.as_deref() {
        rc.logout_url = v.to_owned();
    }
    if let Some(v) = s.admin_url.as_deref() {
        rc.admin_url = v.to_owned();
    }
    if let Some(v) = s.audit_url.as_deref() {
        rc.audit_url = v.to_owned();
    }
    if let Some(v) = s.static_url.as_deref() {
        rc.static_url = v.to_owned();
    }
    if let Some(v) = s.brand_url.as_deref() {
        rc.brand_url = v.to_owned();
    }
    if let Some(v) = s.change_password_url.as_deref() {
        rc.change_password_url = v.to_owned();
    }
    if let Some(v) = s.impersonation_handoff_url.as_deref() {
        rc.impersonation_handoff_url = v.to_owned();
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cli = Cli::new().bind("0.0.0.0:8080"); // pin past any inherited RUSTANGO_BIND
        assert_eq!(cli.bind, "0.0.0.0:8080");
        assert_eq!(cli.migrations_dir, std::path::PathBuf::from("./migrations"));
        assert!(!cli.tenancy);
        #[cfg(feature = "postgres")]
        assert!(cli.seed.is_none());
    }

    #[test]
    fn builder_methods_chain() {
        let cli = Cli::new()
            .bind("127.0.0.1:7777")
            .migrations_dir("custom/migrations")
            .tenancy();
        assert_eq!(cli.bind, "127.0.0.1:7777");
        assert_eq!(
            cli.migrations_dir,
            std::path::PathBuf::from("custom/migrations")
        );
        assert!(cli.tenancy);
    }

    #[test]
    fn seed_hook_stored() {
        let cli = Cli::new().seed(|_pool| async { Ok(()) });
        assert!(cli.seed.is_some());
    }

    /// `Cli::with_settings` honors `Settings.server.bind` when
    /// `RUSTANGO_BIND` env isn't set (#87 wiring).
    #[cfg(feature = "config")]
    #[test]
    fn with_settings_picks_up_server_bind() {
        // We can't unset RUSTANGO_BIND mid-test (the workspace bans
        // unsafe std::env::set_var). Skip the assertion when the
        // test runner has it set — the priority guard is exercised
        // separately via the `_env_wins_over_settings` test.
        if std::env::var("RUSTANGO_BIND").is_ok() {
            return;
        }
        let mut s = crate::config::Settings::default();
        s.server.bind = Some("127.0.0.1:9090".into());
        let cli = Cli::new().with_settings(&s);
        assert_eq!(cli.bind, "127.0.0.1:9090");
    }

    /// Settings.server.bind = None doesn't clobber the existing
    /// bind value — the field is `Option`-typed, missing keys fall
    /// through.
    #[cfg(feature = "config")]
    #[test]
    fn with_settings_unset_bind_preserves_existing() {
        let s = crate::config::Settings::default(); // .server.bind == None
        let cli = Cli::new().bind("127.0.0.1:5555").with_settings(&s);
        assert_eq!(cli.bind, "127.0.0.1:5555");
    }

    /// Explicit `.bind(...)` after `.with_settings(...)` wins —
    /// the most-specific call site beats any earlier resolution.
    #[cfg(feature = "config")]
    #[test]
    fn explicit_bind_after_with_settings_wins() {
        let mut s = crate::config::Settings::default();
        s.server.bind = Some("127.0.0.1:9090".into());
        let cli = Cli::new().with_settings(&s).bind("127.0.0.1:1111");
        assert_eq!(cli.bind, "127.0.0.1:1111");
    }

    /// `Settings.routes.legacy_preset = true` makes
    /// `Cli::with_settings` produce a RouteConfig matching the v0.28
    /// `__`-prefixed shape — without any code-side .routes() call.
    #[cfg(all(feature = "config", feature = "tenancy"))]
    #[test]
    fn with_settings_routes_legacy_preset() {
        let mut s = crate::config::Settings::default();
        s.routes.legacy_preset = Some(true);
        let cli = Cli::new().tenancy().with_settings(&s);
        let rc = cli.routes.expect("routes set by with_settings");
        assert_eq!(rc.login_url, "/__login");
        assert_eq!(rc.admin_url, "/__admin");
    }

    /// Per-field overrides in TOML layer on top of the chosen preset.
    #[cfg(all(feature = "config", feature = "tenancy"))]
    #[test]
    fn with_settings_routes_per_field_override() {
        let mut s = crate::config::Settings::default();
        s.routes.admin_url = Some("/manage".into());
        s.routes.login_url = Some("/sign-in".into());
        let cli = Cli::new().tenancy().with_settings(&s);
        let rc = cli.routes.expect("routes set");
        assert_eq!(rc.admin_url, "/manage");
        assert_eq!(rc.login_url, "/sign-in");
        // Non-overridden fields fall through to the friendly default.
        assert_eq!(rc.audit_url, "/audit");
        assert_eq!(rc.impersonation_handoff_url, "/_impersonation_handoff");
    }

    /// An explicit `.routes(custom)` call BEFORE `.with_settings(...)`
    /// is preserved as the base — TOML overrides layer on top.
    #[cfg(all(feature = "config", feature = "tenancy"))]
    #[test]
    fn explicit_routes_then_with_settings_layers_overrides() {
        use crate::tenancy::RouteConfig;
        let mut base = RouteConfig::legacy();
        base.basic_auth_realm = "MyApp".into();
        let mut s = crate::config::Settings::default();
        s.routes.admin_url = Some("/console".into());
        let cli = Cli::new().tenancy().routes(base).with_settings(&s);
        let rc = cli.routes.expect("routes set");
        assert_eq!(
            rc.basic_auth_realm, "MyApp",
            "explicit .routes() base preserved"
        );
        assert_eq!(rc.admin_url, "/console", "TOML override applied");
        assert_eq!(
            rc.login_url, "/__login",
            "non-overridden legacy field preserved"
        );
    }

    #[test]
    fn default_impl_matches_new() {
        let a = Cli::default();
        let b = Cli::new();
        assert_eq!(a.bind, b.bind);
        assert_eq!(a.migrations_dir, b.migrations_dir);
        assert_eq!(a.tenancy, b.tenancy);
    }

    /// `Cli::with_settings` stashes the Settings clone so runserver
    /// can apply security/cors/access_log/body_limit layers on top
    /// of the user's API router. Without `.with_settings`, the
    /// handle stays None — projects not using the layered loader
    /// pay no overhead.
    #[cfg(feature = "config")]
    #[test]
    fn with_settings_stashes_handle_for_runtime_layering() {
        let s = crate::config::Settings::default();
        let cli = Cli::new().with_settings(&s);
        assert!(
            cli.settings_for_layers.is_some(),
            "with_settings must stash the handle so runserver can apply layers"
        );

        let cli_no_settings = Cli::new();
        assert!(cli_no_settings.settings_for_layers.is_none());
    }

    /// `apply_settings_layers` runs to completion on default
    /// settings without panicking on axum layer-stacking
    /// constraints. End-to-end header-presence assertions live in
    /// the per-module smoke tests for each layer.
    #[cfg(feature = "config")]
    #[test]
    fn apply_settings_layers_smoke() {
        let s = crate::config::Settings::default();
        let router: Router = Router::new();
        let _ = apply_settings_layers(router, &s);
    }

    /// `Cli::with_health` flips the flag for the runserver path.
    #[test]
    fn with_health_flips_flag() {
        let cli_default = Cli::new();
        assert!(!cli_default.health_endpoints, "default off");
        let cli_with = Cli::new().with_health();
        assert!(cli_with.health_endpoints);
    }

    /// `Cli::with_static` accumulates `(prefix, root_dir)` entries —
    /// repeating the call mounts more than one directory and the
    /// order is preserved.
    #[cfg(feature = "admin")]
    #[test]
    fn with_static_accumulates_in_order() {
        let cli = Cli::new()
            .with_static("/static", "./assets")
            .with_static("/uploads", "./var/uploads");
        assert_eq!(cli.static_dirs.len(), 2);
        assert_eq!(cli.static_dirs[0].0, "/static");
        assert_eq!(cli.static_dirs[0].1, std::path::PathBuf::from("./assets"));
        assert_eq!(cli.static_dirs[1].0, "/uploads");
        assert_eq!(
            cli.static_dirs[1].1,
            std::path::PathBuf::from("./var/uploads")
        );
    }

    /// `Cli::with_welcome()` flips the flag for the runserver path.
    #[test]
    fn with_welcome_flips_flag() {
        let cli_default = Cli::new();
        assert!(!cli_default.welcome_page, "default off");
        let cli_with = Cli::new().with_welcome();
        assert!(cli_with.welcome_page);
    }

    /// v0.30.15 fix — `try_mount_welcome` returns the original
    /// router unchanged (no panic) when the user's api already
    /// routes `GET /`. Pre-fix this aborted the process at boot
    /// for any tenancy project with a per-tenant `/` handler.
    #[test]
    fn try_mount_welcome_skips_on_root_collision_no_panic() {
        use axum::routing::get;
        async fn user_root() -> &'static str {
            "user index"
        }
        let api = Router::new().route("/", get(user_root));
        // Returns without panicking — the inner merge would have.
        let _ = try_mount_welcome(api);
    }

    /// `try_mount_welcome` mounts welcome cleanly when no
    /// conflict exists (the common case for fresh projects with
    /// no root handler).
    #[test]
    fn try_mount_welcome_succeeds_on_empty_router() {
        let api = Router::new();
        let _ = try_mount_welcome(api);
    }

    /// `Cli::with_logging()` flips the install flag — the actual
    /// install only happens at `run()` time so this is a pure
    /// builder check.
    #[cfg(all(feature = "config", feature = "runtime"))]
    #[test]
    fn with_logging_flips_install_flag() {
        let cli_default = Cli::new();
        assert!(!cli_default.install_logging, "default off");
        let cli_with = Cli::new().with_logging();
        assert!(cli_with.install_logging);
    }

    /// `Cli::with_csrf()` flips the flag from `None` to `Some(default)`.
    /// `with_csrf_config(...)` lets callers override.
    #[cfg(feature = "csrf")]
    #[test]
    fn with_csrf_flips_flag() {
        let cli_default = Cli::new();
        assert!(cli_default.csrf.is_none(), "default off");

        let cli_with = Cli::new().with_csrf();
        let csrf = cli_with.csrf.expect("with_csrf should set csrf");
        assert_eq!(csrf.cookie_name, crate::forms::csrf::CSRF_COOKIE);
        assert!(!csrf.secure, "default Secure=false for dev over HTTP");
    }

    /// `with_csrf_config` overrides the default — verify the explicit
    /// values land on the stored config.
    #[cfg(feature = "csrf")]
    #[test]
    fn with_csrf_config_threads_overrides() {
        let cli = Cli::new().with_csrf_config(crate::forms::csrf::CsrfConfig {
            cookie_name: "custom_csrf".into(),
            header_name: "X-Custom-CSRF".into(),
            secure: true,
        });
        let csrf = cli.csrf.expect("with_csrf_config should set csrf");
        assert_eq!(csrf.cookie_name, "custom_csrf");
        assert_eq!(csrf.header_name, "X-Custom-CSRF");
        assert!(csrf.secure);
    }

    /// `mount_static_dirs` actually serves a file from the configured
    /// prefix end-to-end. Catches regressions like nesting the wrong
    /// router or forgetting the leading slash on the prefix.
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn mount_static_dirs_serves_a_file() {
        use axum::body::Body;
        use axum::http::Request;
        use std::io::Write;
        use tempfile::TempDir;
        use tower::ServiceExt;

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        std::fs::File::create(&p).unwrap().write_all(b"hi").unwrap();

        let app = mount_static_dirs(
            Router::new(),
            &[("/static".into(), dir.path().to_path_buf())],
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/static/hello.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}
