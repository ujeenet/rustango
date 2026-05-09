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

use crate::sql::sqlx::PgPool;

/// Boxed seed-hook future. Keeps the public method signature simple
/// while accepting any `async fn(&PgPool) -> Result<…>` closure.
type SeedFut<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error>>> + Send + 'a>>;
type SeedFn = Box<dyn for<'a> FnOnce(&'a PgPool) -> SeedFut<'a> + Send>;

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
    #[cfg(feature = "tenancy")]
    init_tenancy_fn: crate::tenancy::manage::InitTenancyFn,
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
            #[cfg(feature = "tenancy")]
            init_tenancy_fn: crate::tenancy::init_tenancy,
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
    /// is off).
    #[must_use]
    pub fn seed<F, Fut>(mut self, hook: F) -> Self
    where
        F: for<'a> FnOnce(&'a PgPool) -> Fut + Send + 'static,
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
        let args: Vec<String> = std::env::args().skip(1).collect();
        let verb = args.first().map_or("", String::as_str);

        match verb {
            "" | "runserver" => self.runserver().await,
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

        #[cfg(feature = "tenancy")]
        if self.tenancy {
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
        #[cfg(not(feature = "tenancy"))]
        if self.tenancy {
            return Err("Cli::tenancy() requires the `tenancy` feature".into());
        }

        let pool = if no_db_verb {
            PgPool::connect_lazy(&url)?
        } else {
            PgPool::connect(&url).await?
        };
        crate::migrate::manage::run(&pool, &self.migrations_dir, args).await?;
        Ok(())
    }

    async fn runserver(self) -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(feature = "tenancy")]
        if self.tenancy {
            return self.runserver_tenancy().await;
        }
        #[cfg(not(feature = "tenancy"))]
        if self.tenancy {
            return Err("Cli::tenancy() requires the `tenancy` feature".into());
        }
        let url = std::env::var("DATABASE_URL").map_err(|_| {
            "missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`."
        })?;
        let pool = PgPool::connect(&url).await?;
        let _ = crate::migrate::migrate(&pool, &self.migrations_dir).await?;
        if let Some(seed) = self.seed {
            seed(&pool).await?;
        }
        let app = self.api.layer(axum::Extension(pool));
        let listener = tokio::net::TcpListener::bind(&self.bind).await?;
        eprintln!("server listening on http://{}", listener.local_addr()?);
        axum::serve(listener, app).await?;
        Ok(())
    }

    #[cfg(feature = "tenancy")]
    async fn runserver_tenancy(self) -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = crate::server::Builder::from_env().await?.api(self.api);
        if let Some(routes) = self.routes {
            builder = builder.routes(routes);
        }
        if let Some(seed) = self.seed {
            // Tenancy Builder's seed_with takes (Arc<TenantPools>, PgPool,
            // String); we forward the registry pool and discard the rest.
            builder = builder
                .seed_with(move |_pools, registry, _url| async move { seed(&registry).await })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cli = Cli::new().bind("0.0.0.0:8080"); // pin past any inherited RUSTANGO_BIND
        assert_eq!(cli.bind, "0.0.0.0:8080");
        assert_eq!(cli.migrations_dir, std::path::PathBuf::from("./migrations"));
        assert!(!cli.tenancy);
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

    #[test]
    fn default_impl_matches_new() {
        let a = Cli::default();
        let b = Cli::new();
        assert_eq!(a.bind, b.bind);
        assert_eq!(a.migrations_dir, b.migrations_dir);
        assert_eq!(a.tenancy, b.tenancy);
    }
}
