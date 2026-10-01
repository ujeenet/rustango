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
//!
//! [`Cli::tenancy`]: crate::manage::Cli::tenancy

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use axum::Router;

/// Boxed seed-hook future. Keeps the public method signature simple
/// while accepting any `async fn(&Pool) -> Result<…>` closure.
type SeedFut<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;
type SeedFn = Box<dyn for<'a> FnOnce(&'a crate::sql::Pool) -> SeedFut<'a> + Send>;

/// Work to run after the server stops accepting connections and its
/// in-flight requests have finished. See [`Cli::on_shutdown`].
type ShutdownFut = Pin<Box<dyn Future<Output = ()> + Send>>;
type ShutdownFn = Box<dyn FnOnce() -> ShutdownFut + Send>;

/// A router built from the serving pool. See [`Cli::nest_with`].
type NestFn = Box<dyn FnOnce(crate::sql::Pool) -> Router + Send>;

/// Run the [`Cli::on_shutdown`] hook, if one was registered.
async fn run_shutdown_hook(hook: Option<ShutdownFn>) {
    if let Some(hook) = hook {
        tracing::info!(target: "rustango::shutdown", "running shutdown hook");
        hook().await;
    }
}

/// One-builder dispatcher. Hand it your API router (and optionally a
/// seed hook), call [`Cli::run`], and you're done.
#[must_use = "Cli does nothing until .run() is awaited"]
pub struct Cli {
    api: Router,
    seed: Option<SeedFn>,
    /// Ran after graceful shutdown drains the server, before `run`
    /// returns. Set via [`Cli::on_shutdown`] — the only place a job
    /// queue's `shutdown()` can actually execute (#1409).
    on_shutdown: Option<ShutdownFn>,
    /// Routers built from the serving pool, by mount path.
    nested: Vec<(String, NestFn)>,
    bind: String,
    migrations_dir: PathBuf,
    tenancy: bool,
    /// Optional override for the framework's reserved URL prefixes
    /// (`/__login`, `/__admin`, `/__audit`, …). Plumbed through to
    /// [`crate::server::Builder::routes`] when [`Cli::tenancy`] is on.
    /// `None` keeps the v0.27 defaults.
    #[cfg(feature = "tenancy")]
    routes: Option<crate::tenancy::RouteConfig>,
    /// See [`Cli::tenant_header`].
    #[cfg(feature = "tenancy")]
    tenant_header: Option<crate::tenancy::HeaderResolver>,
    /// Bootstrap initializer used by the `init-tenancy` verb when
    /// [`Cli::tenancy`] is on. Defaults to
    /// [`crate::tenancy::init_tenancy`]; replaced by [`Cli::user_model`]
    /// to swap in a custom [`crate::tenancy::TenantUserModel`].
    #[cfg(feature = "tenancy")]
    init_tenancy_fn: crate::tenancy::manage::InitTenancyFn,
    /// Cloned [`Settings`] handle stored by [`Cli::with_settings`].
    /// Consumed at `runserver` time to apply layers (security_headers,
    /// CORS, access_log, body_limit) on top of the user's API
    /// router so a single `with_settings_from_env()` call drives
    /// the whole stack.
    #[cfg(feature = "config")]
    settings_for_layers: Option<crate::config::Settings>,
    /// A config that exists but does not load; `run` refuses to start (#1927).
    #[cfg(feature = "config")]
    settings_error: Option<crate::config::ConfigError>,
    /// When `true`, mounts `/health` + `/ready` endpoints on the
    /// API router at runserver time. Set via [`Cli::with_health`].
    /// Default `false` because operators sometimes want their own
    /// health endpoint shape (custom JSON, additional checks).
    health_endpoints: bool,
    /// Migrations dir handed to the operator console so it can
    /// create tenants (#1322). `None` = the create routes are not
    /// mounted. See `Cli::with_tenant_provisioning`.
    provisioning_dir: Option<std::path::PathBuf>,
    /// Per-tenant pool sizing. `None` = `TenantPoolsConfig::default()`.
    /// Set via [`Cli::with_tenant_pools`] (#1456).
    #[cfg(feature = "tenancy")]
    tenant_pools: Option<crate::tenancy::TenantPoolsConfig>,
    /// Mounts registered via [`Cli::with_static`] and [`Cli::with_uploads`],
    /// nested at `runserver` time as `Router::nest(prefix, static_router(files))`.
    /// Empty by default — projects that already mount their own
    /// `static_files::static_router` keep doing it.
    #[cfg(feature = "admin")]
    static_dirs: Vec<(String, crate::static_files::StaticFiles)>,
    /// CSRF middleware config registered via [`Cli::with_csrf`]. `None`
    /// means no CSRF layer mounted — the right default for pure JSON
    /// APIs that authenticate via JWT and reject form-encoded bodies
    /// at the deserializer layer. Form-driven apps opt in; the
    /// `template_views` routers carry their own layer.
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
            on_shutdown: None,
            nested: Vec::new(),
            bind: std::env::var("RUSTANGO_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            migrations_dir: PathBuf::from("./migrations"),
            tenancy: false,
            #[cfg(feature = "tenancy")]
            routes: None,
            #[cfg(feature = "tenancy")]
            tenant_header: None,
            #[cfg(feature = "tenancy")]
            init_tenancy_fn: crate::tenancy::init_tenancy,
            #[cfg(feature = "config")]
            settings_for_layers: None,
            #[cfg(feature = "config")]
            settings_error: None,
            health_endpoints: false,
            provisioning_dir: None,
            #[cfg(feature = "tenancy")]
            tenant_pools: None,
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

    /// Opt in to resolving the tenant from a header (`X-Org`) when no
    /// host matched; off by default (#1856).
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_header(mut self, resolver: crate::tenancy::HeaderResolver) -> Self {
        self.tenant_header = Some(resolver);
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

    /// Nest, at `path`, a router built from the pool `runserver` opens.
    /// Other verbs never build it, so they run without a database (#1216).
    ///
    /// ```ignore
    /// Cli::new().api(urls::api()).nest_with("/admin", urls::admin_router)
    /// ```
    ///
    /// Single-database serving only: `runserver` refuses it with
    /// [`Cli::tenancy`], whose pool is the registry's.
    #[must_use]
    pub fn nest_with<F>(mut self, path: impl Into<String>, router: F) -> Self
    where
        F: FnOnce(crate::sql::Pool) -> Router + Send + 'static,
    {
        self.nested.push((path.into(), Box::new(router)));
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
        Fut: Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'static,
    {
        self.seed = Some(Box::new(move |pool| Box::pin(hook(pool))));
        self
    }

    /// Run a hook after the server has drained, before [`Cli::run`]
    /// returns — draining a job queue, flushing a metrics exporter,
    /// closing a pool.
    ///
    /// This is where `queue.shutdown()` belongs (#1409). Putting it
    /// *after* `run()` — as `docs/jobs.md` used to — could never work:
    /// nothing handled SIGTERM, so the process was killed outright and
    /// no line after `run()` ever executed.
    ///
    /// ```ignore
    /// let queue = Arc::new(InMemoryJobQueue::with_workers(4));
    /// let q = Arc::clone(&queue);
    /// Cli::new().api(routes)
    ///     .on_shutdown(move || async move { q.shutdown().await })
    ///     .run().await
    /// ```
    ///
    /// Runs on SIGINT and SIGTERM alike. It does not run on a crash or
    /// `SIGKILL`, so it is for a clean stop, not a durability guarantee
    /// — work that must survive a hard kill needs a persistent queue.
    #[must_use]
    pub fn on_shutdown<F, Fut>(mut self, hook: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_shutdown = Some(Box::new(move || Box::pin(hook())));
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

    /// Size the per-tenant connection pools (#1456).
    ///
    /// Until this existed, `TenantPoolsConfig` was public and
    /// documented but unreachable from here: every path built
    /// `TenantPools::new(pool)`, which takes
    /// [`crate::tenancy::TenantPoolsConfig::default`], and
    /// `tenancy/pools.rs` reads no environment variables. The
    /// `RUSTANGO_DB_*` knobs are honoured by `sql::Pool` only, so they
    /// did not reach tenant pools either.
    ///
    /// It matters because connections multiply by tenant *and* by
    /// process. Twenty database-mode tenants at the default 16
    /// connections, across a web and a worker process, is 640 — against
    /// a stock `PostgreSQL` limit of 100. With no way to lower it, the
    /// only lever was the database server's own `max_connections`,
    /// which is the wrong place to size an application's pools and is
    /// frequently not the operator's to change.
    ///
    /// ```no_run
    /// use rustango::tenancy::TenantPoolsConfig;
    ///
    /// rustango::manage::Cli::new()
    ///     .tenancy()
    ///     .with_tenant_pools(TenantPoolsConfig {
    ///         database_pool_max_connections: 4,
    ///         max_cached_database_pools: 200,
    ///         ..Default::default()
    ///     });
    /// ```
    ///
    /// Past `max_cached_database_pools` the most idle tenant is
    /// evicted and reconnects on its next request. Set it above your
    /// tenant count to avoid the churn.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn with_tenant_pools(mut self, config: crate::tenancy::TenantPoolsConfig) -> Self {
        self.tenant_pools = Some(config);
        self
    }

    /// Let operators create tenants from the console (#1322) —
    /// `/orgs/new`, the connection probe, and the provisioning run
    /// view + live stream.
    ///
    /// `migrations_dir` is where a new tenant's migrations come from;
    /// the same directory `migrate` uses, usually `"migrations"`.
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .tenancy()
    ///     .with_tenant_provisioning("migrations")
    ///     .run()
    ///     .await
    /// ```
    ///
    /// **Off by default.** Creating a tenant is the most dangerous
    /// thing the console can do — it takes a database URL and
    /// connects to it — and every authenticated operator who can
    /// reach the console can use these routes, because `Operator` has
    /// no permission model. Turning it on is the deployment saying
    /// yes to that.
    ///
    /// Tenancy mode only; ignored without `.tenancy()`.
    #[must_use]
    pub fn with_tenant_provisioning(
        mut self,
        migrations_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.provisioning_dir = Some(migrations_dir.into());
        self
    }

    /// Auto-mount a [`crate::static_files::static_router`] at `prefix`
    /// serving files under `root_dir`. Repeat the call to mount more
    /// than one directory. Mount user uploads with [`Self::with_uploads`].
    ///
    /// ```ignore
    /// rustango::manage::Cli::new()
    ///     .api(urls::api())
    ///     .with_static("/static", "./assets")
    ///     .with_uploads("/uploads", "./var/uploads")
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
        let prefix = prefix.into();
        crate::static_files::warn_if_uploads_prefix(&prefix);
        let files = crate::static_files::StaticFiles::new(root_dir);
        self.static_dirs.push((prefix, files));
        self
    }

    /// [`Self::with_static`] for files users uploaded: HTML, SVG and XML
    /// download instead of running on this origin
    /// ([`crate::static_files::StaticFiles::user_content`]).
    #[cfg(feature = "admin")]
    #[must_use]
    pub fn with_uploads(mut self, prefix: impl Into<String>, root_dir: impl Into<PathBuf>) -> Self {
        let files = crate::static_files::StaticFiles::new(root_dir).user_content();
        self.static_dirs.push((prefix.into(), files));
        self
    }

    /// Auto-mount the [`crate::forms::csrf::CsrfLayer`] on the API
    /// router at `runserver` time using
    /// [`crate::forms::csrf::CsrfConfig::default`]. It enforces the
    /// token on POST/PUT/PATCH/DELETE for hand-written form handlers;
    /// the `template_views` routers already check it themselves.
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

        // Pool sizing + timeouts, applied by every pool this process
        // opens. Process-wide and first-call-wins, like the cookie
        // policy below, because the pools it tunes are built later by
        // verbs that never see `Settings`. Env wins over these values —
        // the same precedence `bind` uses just above.
        //
        // Call this as early as possible: a pool opened before it lands
        // runs on environment defaults, and `configure_pools` warns when
        // that has happened rather than leaving it to be discovered
        // under load (#1373).
        let _ = crate::sql::configure_pools(s.database.pool_tuning());

        // Settings.routes → RouteConfig. Build the right preset
        // (friendly default / legacy v0.28) and apply per-field
        // overrides on top, so the TOML can mix-and-match.
        // Single-tenant builds (no `tenancy` feature) skip this
        // branch — RouteConfig is a tenancy-only construct.
        #[cfg(feature = "tenancy")]
        {
            self.routes = Some(routes_from_settings(&s.routes, self.routes.take()));
        }

        // Audit N2 — set the tenancy console-cookie `Secure` policy
        // fail-closed: cookies are `Secure` unless `security.secure_cookies`
        // is explicitly `false` (e.g. dev_settings.toml for local HTTP).
        // Process-wide so the operator + tenant consoles honor it without
        // per-Builder wiring; first call wins, like the other boot globals.
        // Same gate as `crate::session`: without it there is no cookie to mark.
        #[cfg(any(feature = "admin", feature = "tenancy", feature = "csrf"))]
        let _ = crate::session::set_secure_cookies(s.security.secure_cookies.unwrap_or(true));

        // #1609 / #1732 — login limits and the hash-slot wait; code-side config wins.
        apply_login_settings(&s.auth);

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
    /// With no `config/default.toml` the [`Cli`] runs on its defaults.
    /// Any other load error (bad TOML, a value of the wrong type, a bad
    /// `RUSTANGO__*` override) makes [`Cli::run`] fail instead (#1927).
    #[cfg(feature = "config")]
    #[must_use]
    pub fn with_settings_from_env(self) -> Self {
        self.with_loaded_settings(crate::config::Settings::load_from_env())
    }

    #[cfg(feature = "config")]
    fn with_loaded_settings(
        mut self,
        loaded: Result<crate::config::Settings, crate::config::ConfigError>,
    ) -> Self {
        match loaded {
            Ok(cfg) => self.with_settings(&cfg),
            Err(e) if e.is_missing_config() => {
                tracing::warn!(target: "rustango::manage", error = %e, "Cli::with_settings_from_env: no config file; running on Cli defaults");
                self
            }
            Err(e) => {
                self.settings_error = Some(e);
                self
            }
        }
    }

    /// The config error `run` refuses to start on, if any.
    #[cfg(feature = "config")]
    fn boot_settings_error(&self) -> Option<&crate::config::ConfigError> {
        self.settings_error.as_ref()
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
    #[cfg(feature = "tenancy")]
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
        #[cfg(feature = "config")]
        if let Some(e) = self.boot_settings_error() {
            return Err(format!("refusing to start on a broken config: {e}").into());
        }
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

    /// The one place a tenancy verb gets its `TenantPools`, on every backend:
    /// the SQLite / MySQL arms each built their own and dropped
    /// `with_tenant_pools` and `user_model` (#1456, #1914).
    #[cfg(feature = "tenancy")]
    async fn run_tenancy_verb<DB: crate::sql::sqlx::Database>(
        &self,
        registry: crate::sql::sqlx::Pool<DB>,
        url: &str,
        args: Vec<String>,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        crate::sql::Pool: From<crate::sql::sqlx::Pool<DB>>,
    {
        self.run_tenancy_verb_to(registry, url, args, &mut std::io::stdout())
            .await
    }

    #[cfg(feature = "tenancy")]
    async fn run_tenancy_verb_to<DB: crate::sql::sqlx::Database, W: std::io::Write + Send>(
        &self,
        registry: crate::sql::sqlx::Pool<DB>,
        url: &str,
        args: Vec<String>,
        out: &mut W,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        crate::sql::Pool: From<crate::sql::sqlx::Pool<DB>>,
    {
        let mut pools = crate::tenancy::TenantPools::new(registry);
        if let Some(cfg) = self.tenant_pools.clone() {
            pools = pools.config(cfg);
        }
        crate::tenancy::manage::run_with_writer_and_init(
            &pools,
            url,
            &self.migrations_dir,
            args,
            out,
            self.init_tenancy_fn,
        )
        .await?;
        Ok(())
    }

    async fn dispatch(mut self, args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
        // `dbshell` needs DATABASE_URL but NOT a sqlx pool — it execs
        // the native client (psql / mysql / sqlite3). Handle it before
        // the pool dance so it works even when sqlx can't connect
        // (e.g. tunnel-only setups, fresh-clone scenarios).
        if matches!(args.first().map(String::as_str), Some("dbshell")) {
            let url = std::env::var("DATABASE_URL").map_err(|_| -> Box<dyn std::error::Error> {
                "missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`.".into()
            })?;
            // On Unix `dbshell::run` swaps the process and never
            // returns; on other targets it returns after the client
            // exits. Either way, propagate the result.
            let _ = crate::dbshell::run(&url)?;
            return Ok(());
        }

        // Verbs that need no database at all run here, before any pool is
        // constructed (#1216). `connect_lazy` doesn't connect, but it does
        // validate the URL scheme against the enabled backend features — so a
        // SQLite-only build with a stale `postgres://` in the environment could
        // not print its own `--help`. None of these verbs read or write a
        // database, so none of them should care what `DATABASE_URL` says.
        {
            let mut out = std::io::stdout();

            // A tenancy project has its own help — it lists `create-tenant`,
            // `create-operator`, `create-user` and the rest of the tenancy
            // verbs, which the plain migration runner knows nothing about.
            // Route help there before the generic pool-free dispatch, or a
            // tenancy project would print the wrong verb list. (Caught by
            // `cookbook_blog`'s `cli_help_works_without_database_url`.)
            //
            // `write_help` needs no pool either, so tenancy projects get the
            // same "help works whatever DATABASE_URL says" fix (#1216).
            #[cfg(feature = "tenancy")]
            if self.tenancy
                && matches!(
                    args.first().map(String::as_str),
                    None | Some("") | Some("help") | Some("--help") | Some("-h")
                )
            {
                crate::tenancy::manage::write_help(&mut out)?;
                return Ok(());
            }

            if let Some(res) = crate::migrate::manage::run_pool_free(&args, &mut out) {
                res?;
                return Ok(());
            }
        }

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
                    crate::sql::Pool::connect_sqlite_lazy(&url)?
                } else {
                    crate::sql::Pool::connect_sqlite(&url).await?
                };
                return self.run_tenancy_verb(pool, &url, args).await;
            }
            #[cfg(feature = "mysql")]
            if scheme == "mysql" {
                let pool = if no_db_verb {
                    crate::sql::Pool::connect_mysql_lazy(&url)?
                } else {
                    crate::sql::Pool::connect_mysql(&url).await?
                };
                return self.run_tenancy_verb(pool, &url, args).await;
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
                crate::sql::Pool::connect_postgres_lazy(&url)?
            } else {
                crate::sql::Pool::connect_postgres(&url).await?
            };
            return self.run_tenancy_verb(pool, &url, args).await;
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
                    crate::sql::Pool::connect_sqlite_lazy(&url)?
                } else {
                    crate::sql::Pool::connect_sqlite(&url).await?
                };
                return self.run_tenancy_verb(p, &url, args).await;
            }
            #[cfg(all(not(feature = "sqlite"), feature = "mysql"))]
            {
                let p = if no_db_verb {
                    crate::sql::Pool::connect_mysql_lazy(&url)?
                } else {
                    crate::sql::Pool::connect_mysql(&url).await?
                };
                return self.run_tenancy_verb(p, &url, args).await;
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
        // `redact`, not `url`: this string goes to a terminal and into
        // whatever log is capturing it, and a DATABASE_URL has a
        // password in it. The error itself already names the endpoint
        // it tried (see `sql::connect_diagnosis`), so nothing useful is
        // lost.
        let shown = crate::sql::connect_diagnosis::redact(&url);
        let pool = if no_db_verb {
            crate::sql::Pool::connect_lazy(&url)
                .map_err(|e| format!("connect_lazy({shown}): {e}").into())
                as Result<_, Box<dyn std::error::Error>>
        } else {
            crate::sql::Pool::connect(&url)
                .await
                .map_err(|e| format!("connect({shown}): {e}").into())
                as Result<_, Box<dyn std::error::Error>>
        }?;
        if args.first().map(String::as_str) == Some("check") {
            self.build_nested(&pool);
        }
        crate::migrate::manage::run(&pool, &self.migrations_dir, args).await?;
        Ok(())
    }

    /// Build, and drop, every [`Cli::nest_with`] router, so `check --deploy`
    /// audits the admins `runserver` would mount (#1627).
    fn build_nested(&mut self, pool: &crate::sql::Pool) {
        for (_, build) in std::mem::take(&mut self.nested) {
            drop(build(pool.clone()));
        }
    }

    /// Everything between "the pool is open" and "bind the socket":
    /// welcome page, health endpoints, static dirs, CSRF, the settings
    /// layers, and the pool extension.
    ///
    /// One function, because `runserver` has **three** serving paths —
    /// a non-Postgres build, a Postgres build on a `postgres://` URL,
    /// and a multi-backend build on a non-PG URL (#560) — and each used
    /// to assemble its own router from the same list of steps.
    ///
    /// That duplication *was* #1457. One copy never read
    /// `health_endpoints`, so `/health` 404'd; the fix corrected two of
    /// the three, and the third — the one the soak fleet's own
    /// `--features postgres,mysql,sqlite` image takes on a `sqlite://`
    /// URL — went on answering 404. Three copies of a list of steps is
    /// a list of steps that will drift again, so there is one now.
    ///
    /// Generic over the pool type because the Postgres path extends a
    /// `PgPool` (handlers taking `Extension<PgPool>` predate the `Pool`
    /// enum) while the other two extend `crate::sql::Pool`.
    /// `server.shutdown_timeout_secs`, or the default (#1883).
    fn drain_timeout(&self) -> std::time::Duration {
        #[cfg(feature = "config")]
        if let Some(s) = &self.settings_for_layers {
            return s.server.drain_timeout();
        }
        crate::shutdown::DEFAULT_DRAIN_TIMEOUT
    }

    fn assemble_app<P>(&mut self, pool: P) -> Router
    where
        P: Clone + Send + Sync + 'static,
        P: Into<crate::sql::Pool>,
    {
        let mut api = std::mem::take(&mut self.api);
        for (path, build) in std::mem::take(&mut self.nested) {
            api = api.nest(&path, build(pool.clone().into()));
        }
        // `_http_layers`, not `admin`: the manage-only `api` template calls
        // `.with_welcome()` / `.with_health()` too (#2013).
        #[cfg(feature = "_http_layers")]
        let api = if self.welcome_page {
            try_mount_welcome(api)
        } else {
            api
        };
        #[cfg(feature = "_http_layers")]
        let api = if self.health_endpoints {
            api.merge(crate::health::health_router(pool.clone()))
        } else {
            api
        };
        #[cfg(feature = "admin")]
        let api = mount_static_dirs(api, &self.static_dirs);
        #[cfg(feature = "csrf")]
        let api = match self.csrf.take() {
            Some(cfg) => api.layer(crate::forms::csrf::with_config(cfg)),
            None => api,
        };
        #[cfg(feature = "config")]
        let settings = self.settings_for_layers.as_ref();
        #[cfg(feature = "config")]
        let api = wrap_outer(apply_settings_layers_or_warn(api, settings));
        let api = self.mount_observability(api);
        api.layer(axum::Extension(pool))
    }

    /// Everything the tenancy builder must put on its outermost router:
    /// observability and the `[security]` outer layers, which layers on
    /// the api router never reach (#1480, #1699, #1700). Both tenancy
    /// serving paths go through here.
    #[cfg(feature = "tenancy")]
    fn tenancy_builder<DB: sqlx::Database>(
        &self,
        builder: crate::server::Builder<DB>,
        outer: Option<OuterLayers>,
    ) -> crate::server::Builder<DB> {
        let mut builder = builder
            .observability(self.access_log_layer())
            .span_redact(self.span_redact_params());
        if let Some(h) = &self.tenant_header {
            builder = builder.header_resolver(h.clone());
        }
        match outer {
            Some(o) => o.apply_to(builder),
            None => builder,
        }
    }

    /// The configured access-log layer, or `None` when
    /// `[logging] access_log = false`.
    ///
    /// Split out of [`Self::mount_observability`] because the
    /// multi-tenant path cannot mount here: it hands its router to
    /// `server::Builder`, which merges the tenant admin in afterwards
    /// and dispatches the operator console on a sibling branch. Layers
    /// applied to the api router never reach either (axum: "routes
    /// added after `layer` is called will not have the middleware
    /// added"). The builder takes this layer and applies it to the
    /// outermost router instead, where every branch inherits it.
    fn access_log_layer(&self) -> Option<crate::access_log::AccessLogLayer> {
        self.access_log_enabled()
            .then(|| self.configured_access_log())
    }

    /// The access-log layer as configured, built whether or not it
    /// will be mounted.
    ///
    /// Separate from [`Self::access_log_layer`] because the span needs
    /// the same redact list even when the log is off, and building it
    /// a second way is how the two drifted apart (#1610).
    fn configured_access_log(&self) -> crate::access_log::AccessLogLayer {
        let log_layer = crate::access_log::AccessLogLayer::default();
        #[cfg(feature = "config")]
        let log_layer = match self.settings_for_layers.as_ref() {
            Some(s) => log_layer.with_audit_settings(&s.audit),
            None => log_layer,
        };
        log_layer
    }

    /// Query params the request span must redact.
    ///
    /// Taken from the configured access log rather than recomputed, so
    /// `[audit] redact_query_params` reaches the span even with
    /// `[logging] access_log = false` (#1610).
    fn span_redact_params(&self) -> Vec<String> {
        self.configured_access_log().redact_query_params
    }

    /// The per-request span and the access log.
    ///
    /// Called from the `assemble_app` serving paths. The two
    /// `runserver_tenancy` variants do **not** call this: they hand
    /// their router to `server::Builder`, which merges the tenant admin
    /// in afterwards and dispatches the operator console on a sibling
    /// branch, so a layer applied here would reach neither. Those paths
    /// pass [`Self::access_log_layer`] to `Builder::observability`,
    /// which applies it to the outermost router instead.
    ///
    /// That five-paths-not-one distinction was missed on the first cut,
    /// and it mattered most exactly where it was missed. This replaced
    /// the access log's old home inside `apply_settings_layers`, so a
    /// multi-tenant app that called `.with_settings_from_env()` went
    /// from having a request log to having none — a regression in the
    /// one project shape #1480 was opened about.
    /// `every_serving_path_is_observable` pins it now.
    ///
    /// Mounting here rather than in the settings layers is still the
    /// point: that path only runs when the app calls
    /// `.with_settings_from_env()`, which the api template does not call,
    /// so the tenant field would be unreachable there (#1480).
    ///
    /// `TracingLayer` had a worse version of the same problem: it built
    /// a correct span carrying tenant, method, path and status, and
    /// nothing in the framework ever mounted it. A `tracing::info!` in a
    /// handler therefore had no enclosing span, so no tenant and no
    /// correlation — which is exactly the "logs arrive as loose traces"
    /// symptom.
    ///
    /// All three layers need `_http_layers`, which `manage` implies, so
    /// the `api` template's build gets them too (#1514).
    fn mount_observability(&self, api: Router) -> Router {
        // Delegates: the mount itself lives in one place, shared with
        // `server::Builder`. The two used to carry near-verbatim copies
        // of the same three ordering rules and had already drifted into
        // opposite relative order.
        crate::access_log::mount_observability(
            api,
            self.access_log_layer(),
            self.span_redact_params(),
        )
    }

    /// `[logging] access_log = false` turns the request log off.
    ///
    /// Default on: a server that logs no requests is a server you cannot
    /// debug, and the previous default — off unless you found the right
    /// builder call — was not a decision anyone made on purpose.
    ///
    fn access_log_enabled(&self) -> bool {
        #[cfg(feature = "config")]
        {
            if let Some(s) = self.settings_for_layers.as_ref() {
                return s.logging.access_log.unwrap_or(true);
            }
        }
        true
    }

    async fn runserver(mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Logging install lives in `run()` (the outermost dispatch
        // point) so the WorkerGuard outlives every runserver +
        // management-verb path uniformly.
        if self.tenancy && !self.nested.is_empty() {
            return Err(
                "Cli::nest_with needs a single-database app; with .tenancy() \
                        the serving pool is the registry's"
                    .into(),
            );
        }
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
            if let Some(seed) = self.seed.take() {
                // The seed hook's error is now `Send + Sync` (so a seed can
                // hold an error across an `.await` without the future losing
                // `Send`); coerce to this fn's `Box<dyn Error>` at the boundary.
                seed(&pool)
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            let drain = self.drain_timeout();
            let app = self.assemble_app(pool);
            let listener = tokio::net::TcpListener::bind(&self.bind).await?;
            eprintln!("server listening on http://{}", listener.local_addr()?);
            crate::shutdown::serve_until_drained(
                |stop| {
                    axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .with_graceful_shutdown(stop)
                },
                drain,
            )
            .await?;
            run_shutdown_hook(self.on_shutdown).await;
            return Ok(());
        }
        #[cfg(feature = "postgres")]
        {
            let url = std::env::var("DATABASE_URL").map_err(|_| {
            "missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`."
        })?;
            // #560 — on a multi-backend build (e.g. `--features
            // postgres,sqlite`) this `cfg(postgres)` arm fires even
            // when `DATABASE_URL` is a `sqlite://` / `mysql://` URL.
            // `PgPool::connect(sqlite://…)` then fails with a cryptic
            // scheme-mismatch error. Detect non-PG URLs and route
            // them through the `Pool::connect` dispatcher (which
            // matches by scheme).
            let pg_scheme = url.starts_with("postgres://") || url.starts_with("postgresql://");
            if !pg_scheme {
                // Multi-backend build with non-PG URL — defer to the
                // shared `Pool::connect` path that dispatches by scheme.
                // Note: the `#[cfg(not(postgres))]` branch above is the
                // single-backend mirror of this; we duplicate the
                // necessary setup here rather than restructure the
                // `#[cfg]` blocks at the top of the fn.
                let pool = crate::sql::Pool::connect(&url).await?;
                let _ = crate::migrate::migrate_pool(&pool, &self.migrations_dir).await?;
                if let Some(seed) = self.seed.take() {
                    seed(&pool)
                        .await
                        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
                }
                let drain = self.drain_timeout();
                let app = self.assemble_app(pool);
                let listener = tokio::net::TcpListener::bind(&self.bind).await?;
                eprintln!("server listening on http://{}", listener.local_addr()?);
                crate::shutdown::serve_until_drained(
                    |stop| {
                        axum::serve(
                            listener,
                            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                        )
                        .with_graceful_shutdown(stop)
                    },
                    drain,
                )
                .await?;
                run_shutdown_hook(self.on_shutdown).await;
                return Ok(());
            }
            let pool = crate::sql::Pool::connect_postgres(&url).await?;
            let _ = crate::migrate::migrate(&pool, &self.migrations_dir).await?;
            if let Some(seed) = self.seed.take() {
                seed(&crate::sql::Pool::from(pool.clone()))
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
            }
            let drain = self.drain_timeout();
            let app = self.assemble_app(pool);
            let listener = tokio::net::TcpListener::bind(&self.bind).await?;
            eprintln!("server listening on http://{}", listener.local_addr()?);
            // v0.30.16 — `into_make_service_with_connect_info` is what
            // populates `ConnectInfo<SocketAddr>` in request extensions.
            // Without it, `access_log` (and any other middleware that
            // reads the peer address) sees "-".
            crate::shutdown::serve_until_drained(
                |stop| {
                    axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .with_graceful_shutdown(stop)
                },
                drain,
            )
            .await?;
            run_shutdown_hook(self.on_shutdown).await;
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
    async fn runserver_tenancy(mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Taken before `self` is picked apart below, so the hook still
        // runs on this path too (#1409) — the tenancy server drains via
        // its own `shutdown_signal`, and dropping the hook here would
        // have left `on_shutdown` working on one path and silently not
        // on the other.
        let on_shutdown = self.on_shutdown.take();
        // `take` rather than a move: `mount_observability` below needs
        // `&self`, and moving the field out would partially move `self`.
        let api = std::mem::take(&mut self.api);
        #[cfg(feature = "_http_layers")]
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
        let settings = self.settings_for_layers.as_ref();
        #[cfg(feature = "config")]
        let (api, outer) = apply_settings_layers_or_warn(api, settings);
        #[cfg(not(feature = "config"))]
        let outer = None;
        // Not `mount_observability` here: this router is about to be
        // merged with the tenant admin and dispatched beside the
        // operator console, and layers applied now would reach neither.
        // The builder applies them to the outermost router instead.
        let mut builder = crate::server::Builder::from_env()
            .await?
            .api(api)
            .drain_timeout(self.drain_timeout());
        builder = self.tenancy_builder(builder, outer);
        if self.health_endpoints {
            builder = builder.with_health();
        }
        if let Some(dir) = self.provisioning_dir.clone() {
            builder = builder.with_tenant_provisioning(dir);
        }
        for (prefix, files) in self.static_dirs {
            builder = builder.with_static_files(prefix, files);
        }
        if let Some(routes) = self.routes {
            builder = builder.routes(routes);
        }
        // #1456 — same reason as the dispatch path above.
        if let Some(cfg) = self.tenant_pools.clone() {
            builder = builder.tenant_pools(cfg);
        }
        if let Some(seed) = self.seed.take() {
            // Tenancy Builder's seed_with takes (Arc<TenantPools>, PgPool,
            // String); we forward the registry pool and discard the rest.
            builder = builder
                .seed_with(move |_pools, registry, _url| async move {
                    seed(&crate::sql::Pool::from(registry)).await
                })
                .await?;
        }
        builder.serve(&self.bind).await?;
        run_shutdown_hook(on_shutdown).await;
        Ok(())
    }

    #[cfg(all(feature = "tenancy", not(feature = "postgres")))]
    async fn runserver_tenancy(mut self) -> Result<(), Box<dyn std::error::Error>> {
        // v0.38 slice 24 — `server::Builder<DB>` is generic. On non-PG
        // single-backend builds, `DefaultTenantDb` resolves to whichever
        // sqlx backend the feature flags picked (sqlite first, mysql
        // otherwise). Database-mode tenants work out of the box;
        // schema-mode tenants return `TenancyError::Validation` at
        // request time (schema-mode is PG-only by language).
        let on_shutdown = self.on_shutdown.take();
        // `take` rather than a move: `mount_observability` below needs
        // `&self`, and moving the field out would partially move `self`.
        let api = std::mem::take(&mut self.api);
        #[cfg(feature = "_http_layers")]
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
        let settings = self.settings_for_layers.as_ref();
        #[cfg(feature = "config")]
        let (api, outer) = apply_settings_layers_or_warn(api, settings);
        #[cfg(not(feature = "config"))]
        let outer = None;
        // Not `mount_observability` — see the dispatch path above. The
        // builder applies these to the outermost router.
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
        builder = self.tenancy_builder(builder, outer);
        if self.health_endpoints {
            builder = builder.with_health();
        }
        if let Some(dir) = self.provisioning_dir.clone() {
            builder = builder.with_tenant_provisioning(dir);
        }
        for (prefix, files) in self.static_dirs {
            builder = builder.with_static_files(prefix, files);
        }
        if let Some(routes) = self.routes {
            builder = builder.routes(routes);
        }
        // #1456 — same reason as the dispatch path above.
        if let Some(cfg) = self.tenant_pools.clone() {
            builder = builder.tenant_pools(cfg);
        }
        if let Some(seed) = self.seed.take() {
            // Mirror the PG arm above (line 828) so sqlite/mysql tenancy
            // projects get their `Cli::seed` hook fired on boot too.
            builder = builder
                .seed_with(move |_pools, registry, _url| async move {
                    seed(&crate::sql::Pool::from(registry)).await
                })
                .await?;
        }
        builder.serve(&self.bind).await?;
        run_shutdown_hook(on_shutdown).await;
        Ok(())
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
#[cfg(feature = "_http_layers")]
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
fn mount_static_dirs(api: Router, dirs: &[(String, crate::static_files::StaticFiles)]) -> Router {
    let mut r = api;
    for (prefix, files) in dirs {
        r = r.nest(prefix, crate::static_files::static_router(files.clone()));
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
/// Apply the settings-driven layers, or — when no settings were installed —
/// warn if the environment nonetheless *configures* some of them (#1192).
///
/// A setting that is absent is a bug you find; a setting that is present,
/// parsed and ignored is a bug you ship. Without
/// [`Cli::with_settings_from_env`] in the builder chain, CORS, the body
/// limit, the request timeout and the security headers are read from the
/// environment and then silently do nothing — reading the config and
/// concluding the headers are being sent is entirely reasonable, and wrong.
/// Naming the missing builder call turns that into a one-line fix.
#[cfg(feature = "config")]
fn apply_settings_layers_or_warn(
    api: Router,
    settings: Option<&crate::config::Settings>,
) -> (Router, Option<OuterLayers>) {
    match settings {
        Some(s) => {
            let (api, outer) = apply_settings_layers(api, s);
            (api, Some(outer))
        }
        None => {
            warn_if_settings_inert();
            (api, None)
        }
    }
}

/// The layer-driving settings that `s` configures, by dotted name.
///
/// Only settings that would actually install a layer count — a configured
/// `secret_key` is not evidence that anyone expected CORS. Pure, so the
/// warning's precision is unit-testable.
#[cfg(feature = "config")]
fn inert_layer_settings(s: &crate::config::Settings) -> Vec<&'static str> {
    let mut inert: Vec<&'static str> = Vec::new();
    if s.server.request_timeout_secs.is_some() {
        inert.push("server.request_timeout_secs");
    }
    if s.server.max_body_bytes.is_some() {
        inert.push("server.max_body_bytes");
    }
    if s.security.headers_preset.is_some() {
        inert.push("security.headers_preset");
    }
    if s.security.csp.is_some() {
        inert.push("security.csp");
    }
    if s.security.hsts_max_age_secs.is_some() {
        inert.push("security.hsts_max_age_secs");
    }
    if !s.security.cors_allowed_origins.is_empty() {
        inert.push("security.cors_allowed_origins");
    }
    inert
}

/// Emit a `WARN` naming every layer-driving setting that is configured in the
/// environment while no settings layer is installed. Silent when nothing is
/// configured (the common case for projects that never used `Settings`).
#[cfg(feature = "config")]
fn warn_if_settings_inert() {
    let s = match crate::config::Settings::load_from_env() {
        Ok(s) => s,
        Err(e) if e.is_missing_config() => return,
        Err(e) => {
            tracing::warn!(target: "rustango::manage", error = %e, "the config does not load; none of it is applied");
            return;
        }
    };
    let inert = inert_layer_settings(&s);
    if inert.is_empty() {
        return;
    }
    tracing::warn!(
        target: "rustango::manage",
        settings = %inert.join(", "),
        "these settings are configured but NOT being applied — no settings layer is \
         installed. Add `.with_settings_from_env()` to the `Cli` builder chain to \
         activate them (CORS, body limit, request timeout, security headers). \
         A `[security] csp` you set also takes effect then, and can break \
         pages that use inline script."
    );
}

#[cfg(feature = "config")]
fn apply_settings_layers(api: Router, s: &crate::config::Settings) -> (Router, OuterLayers) {
    use crate::body_limit::{BodyLimitLayer, BodyLimitRouterExt as _};
    use crate::cors::{CorsLayer, CorsRouterExt as _};
    use crate::request_timeout::{RequestTimeoutLayer, RequestTimeoutRouterExt as _};

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

    // access_log is NOT mounted here any more. It lives in
    // `Cli::mount_observability`, which runs whether or not the app
    // calls `.with_settings_from_env()` — mounting it here made the
    // request log, and with it the tenant field, conditional on a
    // builder call the api template does not make (#1480). The redact
    // list from `[audit] redact_query_params` still reaches the layer;
    // `mount_observability` reads the same settings.

    // CORS — opt-in (returns None when no origins configured).
    if let Some(cors) = CorsLayer::from_settings(&s.security) {
        app = app.cors(cors);
    }

    // The caller owns where these go: the whole router for one tenant,
    // the builder's outermost router under tenancy (#1700).
    (app, outer_layers(&s.security))
}

/// The `[security]` layers that must wrap every route, the tenant login,
/// admin and operator console included (#1699, #1700).
#[cfg(any(feature = "config", feature = "tenancy"))]
#[must_use = "these must wrap the served router, or the server has none of them"]
pub(crate) struct OuterLayers {
    headers: crate::security_headers::SecurityHeadersLayer,
    allowed_hosts: Option<crate::host_validation::AllowedHostsLayer>,
    ssl_redirect: Option<crate::ssl_redirect::SslRedirectLayer>,
}

#[cfg(any(feature = "config", feature = "tenancy"))]
impl OuterLayers {
    /// Innermost first: headers, the HTTPS redirect, then the Host
    /// allowlist, so a bad Host is refused before it is redirected to.
    #[cfg(feature = "config")]
    fn apply(self, mut app: Router) -> Router {
        use crate::host_validation::AllowedHostsRouterExt as _;
        use crate::security_headers::SecurityHeadersRouterExt as _;
        use crate::ssl_redirect::SslRedirectRouterExt as _;
        app = app.security_headers(self.headers);
        if let Some(l) = self.ssl_redirect {
            app = app.ssl_redirect(l);
        }
        if let Some(l) = self.allowed_hosts {
            app = app.allowed_hosts(l);
        }
        app
    }

    #[cfg(feature = "tenancy")]
    fn apply_to<DB: sqlx::Database>(
        self,
        mut b: crate::server::Builder<DB>,
    ) -> crate::server::Builder<DB> {
        b = b.security_headers(self.headers);
        if let Some(l) = self.ssl_redirect {
            b = b.ssl_redirect(l);
        }
        if let Some(l) = self.allowed_hosts {
            b = b.allowed_hosts(l);
        }
        b
    }
}

/// Wrap the single-tenant router in the outer layers, when settings
/// produced any.
#[cfg(feature = "config")]
fn wrap_outer((app, outer): (Router, Option<OuterLayers>)) -> Router {
    match outer {
        Some(o) => o.apply(app),
        None => app,
    }
}

#[cfg(feature = "config")]
fn outer_layers(s: &crate::config::SecuritySettings) -> OuterLayers {
    use crate::host_validation::AllowedHostsLayer;
    use crate::ssl_redirect::SslRedirectLayer;
    // An empty list is the opt-out; the layer would enforce nothing.
    let allowed_hosts = (!s.allowed_hosts.is_empty())
        .then(|| AllowedHostsLayer::from_settings_list(s.allowed_hosts.iter().map(String::as_str)));
    // Opt-in: behind a TLS-terminating LB it is usually not wanted.
    let ssl_redirect = matches!(s.secure_ssl_redirect, Some(true)).then(|| {
        let mut layer = SslRedirectLayer::new();
        // Expect `[header, value]`; `check --deploy` flags other shapes.
        if s.secure_proxy_ssl_header.len() == 2 {
            layer = layer
                .proxy_ssl_header(&s.secure_proxy_ssl_header[0], &s.secure_proxy_ssl_header[1]);
        }
        if !s.secure_redirect_exempt.is_empty() {
            layer = layer.exempt(s.secure_redirect_exempt.iter().cloned());
        }
        layer
    });
    OuterLayers {
        // `SecuritySettings::default()` is strict(), so this always mounts.
        headers: crate::security_headers::SecurityHeadersLayer::from_settings(s),
        allowed_hosts,
        ssl_redirect,
    }
}

/// Install the `[auth]` login limits, lockout policy and hash-slot wait,
/// each only when one of its keys is set. A value the app installed in
/// code wins; the ignored keys are logged.
#[cfg(feature = "config")]
fn apply_login_settings(a: &crate::config::AuthSettings) {
    let ignored = |what: &str| {
        tracing::warn!(
            target: "rustango::manage",
            "[auth] {what} keys are ignored: the app already configured it in code"
        );
    };
    #[cfg(feature = "passwords")]
    if let Some(ms) = a.hash_wait_ms {
        let wait = std::time::Duration::from_millis(ms);
        if !crate::passwords::configure_hash_wait_from_settings(wait) {
            ignored("hash_wait_ms");
        }
    }
    #[cfg(feature = "admin")]
    if a.login_ip_limit.is_some()
        || a.login_ip_window_secs.is_some()
        || a.login_global_limit.is_some()
        || a.login_global_window_secs.is_some()
    {
        use crate::login_throttle::{configure_from_settings, LoginLimits, LoginThrottle};
        if !configure_from_settings(LoginThrottle::new(LoginLimits::from_settings(a))) {
            ignored("login_*");
        }
    }
    #[cfg(feature = "cache")]
    if a.lockout_threshold.is_some() || a.lockout_duration_secs.is_some() {
        use crate::account_lockout::{
            configure_from_settings, Lockout, DEFAULT_LOCKOUT_DURATION_SECS, DEFAULT_MAX_ATTEMPTS,
        };
        let lockout = Lockout::new(std::sync::Arc::new(crate::cache::InMemoryCache::new()))
            .max_attempts(a.lockout_threshold.unwrap_or(DEFAULT_MAX_ATTEMPTS))
            .lockout_duration(std::time::Duration::from_secs(
                a.lockout_duration_secs
                    .unwrap_or(DEFAULT_LOCKOUT_DURATION_SECS),
            ));
        if !configure_from_settings(lockout) {
            ignored("lockout_*");
        }
    }
    #[cfg(not(any(feature = "passwords", feature = "admin", feature = "cache")))]
    let _ = (a, ignored);
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

    /// `with_tenant_pools` reaches a SQLite tenancy verb (#1914): cap 0
    /// skips the one tenant; the default config would warm it.
    #[cfg(all(feature = "tenancy", feature = "sqlite"))]
    #[tokio::test]
    async fn sqlite_tenancy_verbs_honour_with_tenant_pools() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let tenant_url = format!("sqlite://{}?mode=rwc", tmp.path().join("t.db").display());
        let cli = Cli::new()
            .tenancy()
            .migrations_dir(tmp.path().join("migrations"))
            .with_tenant_pools(crate::tenancy::TenantPoolsConfig {
                max_cached_database_pools: 0,
                ..Default::default()
            });
        let registry = crate::sql::Pool::connect_sqlite(&url)
            .await
            .expect("connect");
        let run = |args: &[&str]| args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let mut out: Vec<u8> = Vec::new();
        for args in [
            run(&["migrate-registry"]),
            run(&[
                "create-tenant",
                "acme",
                "--mode",
                "database",
                "--backend",
                "sqlite",
                "--database-url",
                &tenant_url,
                "--no-migrate",
            ]),
            run(&["prewarm-pools"]),
        ] {
            cli.run_tenancy_verb_to(registry.clone(), &url, args, &mut out)
                .await
                .expect("verb");
        }
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains("skipped (cap) 1"), "{out}");
    }

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
    /// One bad value used to boot on Cli defaults — no allowed_hosts,
    /// headers or login limits (#1927). Only a missing file falls back.
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn a_bad_settings_value_fails_boot_but_a_missing_file_does_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.toml"),
            "[security]\nsecure_ssl_redirect = 1\n",
        )
        .unwrap();
        let bad = crate::config::Settings::load_from(dir.path(), "dev");
        assert!(matches!(bad, Err(crate::config::ConfigError::Shape(_))));
        let cli = Cli::new().with_loaded_settings(bad);
        assert!(cli.settings_for_layers.is_none());
        let err = cli
            .boot_settings_error()
            .expect("a bad value must fail boot");
        assert!(err.to_string().contains("secure_ssl_redirect"), "{err}");

        let empty = tempfile::tempdir().unwrap();
        let missing = crate::config::Settings::load_from(empty.path(), "dev");
        assert!(Cli::new()
            .with_loaded_settings(missing)
            .boot_settings_error()
            .is_none());
    }

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
    /// #1192 — the detector must name exactly the layer-driving settings that
    /// are configured, and stay quiet when none are. A false positive would
    /// train people to ignore the warning; a false negative is the original bug.
    #[cfg(feature = "config")]
    #[test]
    fn inert_settings_detector_names_only_configured_layers() {
        use crate::config::Settings;

        // Nothing configured → nothing to warn about.
        assert!(inert_layer_settings(&Settings::default()).is_empty());

        // Each layer-driving field is reported, and only that field.
        let mut s = Settings::default();
        s.security.headers_preset = Some("strict".into());
        assert_eq!(inert_layer_settings(&s), vec!["security.headers_preset"]);

        let mut s = Settings::default();
        s.server.max_body_bytes = Some(1024);
        s.security.cors_allowed_origins = vec!["https://example.com".into()];
        assert_eq!(
            inert_layer_settings(&s),
            vec!["server.max_body_bytes", "security.cors_allowed_origins"]
        );

        // A setting that drives no layer must NOT trigger the warning.
        let mut s = Settings::default();
        s.secret_key = Some("irrelevant".into());
        assert!(inert_layer_settings(&s).is_empty());
    }

    /// #1856 — `Cli::tenant_header` reaches the builder's resolver chain.
    #[cfg(all(feature = "tenancy", feature = "sqlite"))]
    #[tokio::test]
    async fn tenant_header_reaches_the_builder() {
        use crate::server::resolver_tests::{registry, x_org_pick};
        let _iso = crate::tenancy::isolated_resolver().await;
        let (_tmp, sq, url) = registry().await;
        let builder = |cli: Cli| {
            let b = crate::server::Builder::from_pool(sq.clone(), url.clone(), "localhost");
            cli.tenancy_builder(b, None)
        };
        assert_eq!(x_org_pick(&builder(Cli::new()), &sq).await, None);
        let cli = Cli::new().tenant_header(crate::tenancy::HeaderResolver::default());
        assert_eq!(
            x_org_pick(&builder(cli), &sq).await.as_deref(),
            Some("acme")
        );
    }

    /// #1699, #1700 — what both tenancy paths hand the builder, checked
    /// by request: headers, Host allowlist outermost, HTTPS redirect with
    /// its proxy header and exempt paths.
    #[cfg(all(feature = "tenancy", feature = "config", feature = "sqlite"))]
    #[tokio::test]
    async fn tenancy_builder_hands_over_every_outer_layer() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        // Its registry has no `rustango_orgs`, so it opens the global breaker.
        let _iso = crate::tenancy::isolated_resolver().await;

        let mut s = crate::config::Settings::default();
        s.security.allowed_hosts = vec![".localhost".into()];
        s.security.secure_ssl_redirect = Some(true);
        s.security.secure_proxy_ssl_header = vec!["x-forwarded-proto".into(), "https".into()];
        s.security.secure_redirect_exempt = vec!["/app".into()];
        let cli = Cli::new().with_settings(&s);

        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
        let (_, outer) = apply_settings_layers(Router::new(), &s);
        let builder = crate::server::Builder::from_pool(pool, url, "localhost");
        let app = cli
            .tenancy_builder(builder, Some(outer))
            .into_router()
            .await
            .expect("assemble");

        let send = |host: &str, uri: &str, https: bool| {
            let mut req = Request::builder().uri(uri).header("host", host);
            if https {
                req = req.header("x-forwarded-proto", "https");
            }
            app.clone().oneshot(req.body(Body::empty()).unwrap())
        };
        // Refused, not redirected to: the allowlist is outermost.
        let r = send("evil.example", "/login", false).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        // Plain HTTP is redirected; the proxy header and exempt path are not.
        let r = send("acme.localhost", "/login", false).await.unwrap();
        assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
        let r = send("acme.localhost", "/login", true).await.unwrap();
        assert_ne!(r.status(), StatusCode::MOVED_PERMANENTLY);
        assert!(r.headers().contains_key("x-frame-options"));
        let r = send("acme.localhost", "/app", false).await.unwrap();
        assert_ne!(r.status(), StatusCode::MOVED_PERMANENTLY);
    }

    #[cfg(feature = "config")]
    #[test]
    fn apply_settings_layers_smoke() {
        let s = crate::config::Settings::default();
        let router: Router = Router::new();
        let _ = apply_settings_layers(router, &s);
    }

    /// PR #618 — `apply_settings_layers` mounts host_validation +
    /// ssl_redirect when the new `SecuritySettings` fields are set.
    /// Smoke test: the layer chain composes without panicking when
    /// every new field is populated.
    #[cfg(all(feature = "config", feature = "admin"))]
    #[test]
    fn apply_settings_layers_with_security_middlewares_composes() {
        let mut s = crate::config::Settings::default();
        s.security.allowed_hosts = vec!["example.com".into(), ".example.com".into()];
        s.security.secure_ssl_redirect = Some(true);
        s.security.secure_proxy_ssl_header = vec!["X-Forwarded-Proto".into(), "https".into()];
        s.security.secure_redirect_exempt = vec!["/health".into()];
        let router: Router = Router::new();
        let _ = apply_settings_layers(router, &s);
    }

    /// Both layers are opt-in — defaults don't mount them. The
    /// smoke test above proves they CAN mount; this one proves
    /// they DON'T mount unsolicited.
    #[cfg(all(feature = "config", feature = "admin"))]
    #[test]
    fn apply_settings_layers_default_does_not_force_opt_in_layers() {
        // Default Settings → empty allowed_hosts + secure_ssl_redirect = None.
        // The function returns a Router; we can't introspect the
        // tower-layer stack directly, but we can assert that
        // building it with defaults succeeds (host_validation +
        // ssl_redirect branches are skipped without panicking).
        let s = crate::config::Settings::default();
        assert!(s.security.allowed_hosts.is_empty());
        assert!(s.security.secure_ssl_redirect.is_none());
        let _ = apply_settings_layers(Router::new(), &s);
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
        assert_eq!(
            cli.static_dirs[0].1.root(),
            std::path::Path::new("./assets")
        );
        assert_eq!(cli.static_dirs[1].0, "/uploads");
        assert_eq!(
            cli.static_dirs[1].1.root(),
            std::path::PathBuf::from("./var/uploads")
        );
    }

    /// `with_uploads` serves uploaded HTML as a download, `with_static` inline (#1849).
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn with_uploads_downloads_html() {
        use tower::ServiceExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.html"), "<script></script>").unwrap();
        let cli = Cli::new()
            .with_static("/static", dir.path())
            .with_uploads("/uploads", dir.path());
        let app = mount_static_dirs(Router::new(), &cli.static_dirs);
        let get = |uri: &str| {
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(get("/uploads/x.html")).await.unwrap();
        assert_eq!(r.headers()["content-disposition"], "attachment");
        let r = app.oneshot(get("/static/x.html")).await.unwrap();
        assert!(r.headers().get("content-disposition").is_none());
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
    // `try_mount_welcome` is `#[cfg(feature = "admin")]`, so these two follow
    // it. Only visible once `postgres,manage` compiled at all (#1208).
    #[cfg(feature = "admin")]
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
    #[cfg(feature = "admin")]
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
        // v0.43 — default flipped to Secure=true so production isn't
        // foot-gunned by forgetting to enable it. Dev over HTTP opts
        // out via `.allow_insecure_for_dev()`.
        assert!(csrf.secure, "default Secure=true since v0.43");
    }

    #[cfg(feature = "csrf")]
    #[test]
    fn allow_insecure_for_dev_clears_secure_flag() {
        use crate::forms::csrf::CsrfConfig;
        let cfg = CsrfConfig::default().allow_insecure_for_dev();
        assert!(!cfg.secure, "dev opt-out should clear Secure");
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
            trusted_origins: Vec::new(),
            exempt_prefixes: Vec::new(),
            ..Default::default()
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
            &[(
                "/static".into(),
                crate::static_files::StaticFiles::new(dir.path()),
            )],
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

/// `Cli::with_health()` must reach every serving path (#1457).
///
/// Tested on the assembled router rather than on the source text of one
/// `runserver` arm. The source-slicing guard this replaces was wrong
/// three times — it matched the fix's own explanatory comment, then it
/// matched the *Postgres* arm, then it could not see the third serving
/// path at all — because each version checked a proxy for the property
/// instead of the property, which is that a request gets a 200.
#[cfg(all(
    test,
    feature = "admin",
    feature = "sqlite",
    feature = "runserver",
    feature = "manage"
))]
mod assemble_app_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    /// Serializes this module's tests.
    ///
    /// `tracing` caches per callsite whether any subscriber is
    /// interested, and that cache is process-global. The two health
    /// tests issue requests with no subscriber installed, so whichever
    /// reaches `tracing_layer`'s `info_span!` first can cache it as
    /// "nobody cares" — and the request-id test below then finds the
    /// span silently absent. Alone it passed; with the module it failed
    /// every run, and rebuilding the cache was not enough on its own
    /// because a sibling can poison it again a moment later.
    fn global_tracing_state() -> &'static std::sync::Mutex<()> {
        super::tracing_test_lock()
    }

    /// Take the lock, ignoring poisoning from an unrelated failure.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        global_tracing_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn status(app: &Router, path: &str) -> StatusCode {
        app.clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .expect("request")
            .status()
    }

    /// A handler's own log event carries the request id, with the
    /// handler doing nothing to put it there.
    ///
    /// This is the property `request_id` existed for and never had:
    /// `RequestIdLayer` was mounted nowhere, and the module's docs told
    /// you to write `req_id = %id.0` on every call site by hand — which
    /// is both tedious and silently misses every event you did not write,
    /// the ORM's included (#1480).
    ///
    /// Asserted on captured output rather than on the extension, because
    /// the extension was always there. What was missing is the id
    /// reaching the *log*.
    #[test]
    fn a_handler_log_carries_the_request_id_without_asking() {
        let _serial = serialized();
        use tracing_subscriber::layer::SubscriberExt as _;

        // A plain `#[test]` driving its own runtime, not `#[tokio::test]`.
        //
        // `set_default` installs a thread-local subscriber, and holding
        // that across an `.await` in an async test proved racy: the
        // events reached the test's subscriber while the *span* did not,
        // so the captured lines had no span context maybe one run in six.
        // Driving the whole request inside `with_default` keeps one
        // subscriber current for span creation and every poll alike.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let pool = rt
            .block_on(crate::sql::Pool::connect("sqlite::memory:"))
            .expect("sqlite");
        // A handler that logs and says nothing about request ids.
        let app = Cli::new()
            .api(Router::new().route(
                "/thing",
                axum::routing::get(|| async {
                    tracing::info!("handler ran");
                    "ok"
                }),
            ))
            .assemble_app(pool);

        let buf = crate::testkit::CaptureWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(buf.clone()),
        );

        let sent = "test-request-id-42";
        // Rebuild the interest cache before issuing the request.
        //
        // `tracing` caches, per callsite, whether any subscriber cares.
        // The sibling tests in this module issue requests too, and they
        // run with no subscriber at all — so whichever of them reaches
        // the `info_span!` in `tracing_layer` first gets it cached as
        // "nobody is interested", and this test then finds the span
        // silently absent. Alone it passes; with the module it failed
        // every time. A thread-local default does not invalidate that
        // cache on its own.
        tracing::callsite::rebuild_interest_cache();

        let response = tracing::subscriber::with_default(subscriber, || {
            rt.block_on(
                app.oneshot(
                    Request::builder()
                        .uri("/thing")
                        .header("x-request-id", sent)
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
        })
        .expect("request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(sent),
            "the inbound id should be echoed back"
        );

        let out = buf.contents();
        assert!(
            out.contains("handler ran"),
            "the handler's event was not captured at all: {out}"
        );
        assert!(
            out.contains(sent),
            "the handler's event does not carry the request id. It is on the \
             request span, so every event under the span should show it without \
             the handler naming it:\n{out}"
        );
    }

    #[tokio::test]
    async fn with_health_mounts_both_endpoints() {
        let _serial = serialized();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let app = Cli::new().with_health().assemble_app(pool);

        assert_eq!(
            status(&app, "/health").await,
            StatusCode::OK,
            "`with_health()` set its flag and /health still 404s — that is #1457, \
             and it reaches every serving path because they all assemble here"
        );
        assert_eq!(status(&app, "/ready").await, StatusCode::OK);
    }

    /// The other direction. Without this, the test above would pass even
    /// if the endpoints were mounted unconditionally — which would make
    /// it evidence of nothing.
    #[tokio::test]
    async fn without_with_health_they_are_absent() {
        let _serial = serialized();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let app = Cli::new().assemble_app(pool);

        assert_eq!(
            status(&app, "/health").await,
            StatusCode::NOT_FOUND,
            "health endpoints must be opt-in; mounting them always would make \
             the positive test above vacuous"
        );
    }

    /// #1700 — the single-tenant server keeps its `[security]` outer
    /// layers: headers on, and a bad Host refused.
    #[cfg(feature = "config")]
    #[tokio::test]
    async fn assemble_app_applies_the_outer_layers() {
        let _serial = serialized();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let mut s = crate::config::Settings::default();
        s.security.allowed_hosts = vec!["example.com".into()];
        let app = Cli::new()
            .with_settings(&s)
            .with_health()
            .assemble_app(pool);
        let send = |host: &str| {
            let req = Request::builder().uri("/health").header("host", host);
            app.clone().oneshot(req.body(Body::empty()).unwrap())
        };
        let ok = send("example.com").await.expect("request");
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(ok.headers().contains_key("x-frame-options"));
        let bad = send("evil.example").await.expect("request");
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    }
}

/// Shared by every test module here that installs a subscriber or sends
/// a request: `tracing`'s per-callsite interest cache is process-global.
#[cfg(all(test, feature = "sqlite", feature = "manage"))]
fn tracing_test_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

/// #1514 — the request id, span and access log reach a build without
/// `admin` (the `api` template's `manage` alone), not only the batteries ones.
#[cfg(all(test, feature = "sqlite", feature = "manage"))]
mod observability_without_admin_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        super::tracing_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A plain `#[test]` on its own runtime, so one subscriber is current
    /// for the span and every poll (see `assemble_app_tests`).
    #[test]
    fn assembled_app_logs_the_request_with_its_id() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let _serial = serialized();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let pool = rt
            .block_on(crate::sql::Pool::connect("sqlite::memory:"))
            .expect("sqlite");
        let api = Router::new().route(
            "/x",
            axum::routing::get(|| async {
                tracing::info!("handler ran");
                "ok"
            }),
        );
        let app = Cli::new().api(api).assemble_app(pool);
        let buf = crate::testkit::CaptureWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(buf.clone()),
        );
        tracing::callsite::rebuild_interest_cache();
        let sent = "req-1514";
        let res = tracing::subscriber::with_default(subscriber, || {
            rt.block_on(
                app.oneshot(
                    Request::builder()
                        .uri("/x")
                        .header("x-request-id", sent)
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
        })
        .expect("request");
        assert!(
            res.headers().contains_key("x-request-id"),
            "no X-Request-Id on the assembled router: observability is gated off"
        );
        let out = buf.contents();
        let handler_line = out.lines().find(|l| l.contains("handler ran"));
        assert!(
            handler_line.is_some_and(|l| l.contains(sent)),
            "the handler's event is not in the request span: {out}"
        );
        assert!(
            out.lines()
                .any(|l| l.contains("rustango::access_log") && l.contains("/x")),
            "no access-log line: {out}"
        );
    }

    /// `check --deploy` sees an ungated admin mounted by `nest_with`.
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn check_builds_the_nested_admin() {
        let _serial = serialized();
        let _g = crate::admin::ungated_flag_lock().lock().await;
        crate::admin::reset_ungated_admin_built();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        Cli::new()
            .nest_with("/admin", |p| crate::admin::router(p))
            .build_nested(&pool);
        let flagged = crate::admin::ungated_admin_built();
        crate::admin::reset_ungated_admin_built();
        assert!(flagged, "the nested admin was never built for the audit");
    }

    /// #2013 — `.with_welcome()` / `.with_health()` mount on a manage-only build.
    #[tokio::test]
    async fn welcome_and_health_mount_without_admin() {
        let _serial = serialized();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let app = Cli::new().with_welcome().with_health().assemble_app(pool);
        for path in ["/", "/health"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .expect("request");
            assert_eq!(
                res.status(),
                axum::http::StatusCode::OK,
                "{path} not mounted"
            );
        }
    }

    /// `nest_with` builds its router from the serving pool, at assembly.
    #[tokio::test]
    async fn nest_with_builds_from_the_serving_pool() {
        let _serial = serialized();
        let pool = crate::sql::Pool::connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let app = Cli::new()
            .nest_with("/n", |p: crate::sql::Pool| {
                let name = p.dialect().name();
                Router::new().route("/x", axum::routing::get(move || async move { name }))
            })
            .assemble_app(pool);
        let res = app
            .oneshot(Request::builder().uri("/n/x").body(Body::empty()).unwrap())
            .await
            .expect("request");
        let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"sqlite");
    }
}
