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
type SeedFut<'a> = Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error>>> + Send + 'a>>;
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
        }
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
        let url = std::env::var("DATABASE_URL").map_err(|_| {
            "missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env`."
        })?;

        #[cfg(feature = "tenancy")]
        if self.tenancy {
            let pool = PgPool::connect(&url).await?;
            let pools = crate::tenancy::TenantPools::new(pool);
            crate::tenancy::manage::run(&pools, &url, &self.migrations_dir, args).await?;
            return Ok(());
        }
        #[cfg(not(feature = "tenancy"))]
        if self.tenancy {
            return Err("Cli::tenancy() requires the `tenancy` feature".into());
        }

        // Single-tenant verbs that don't need a real pool (help,
        // makemigrations, scaffolding, etc.) tolerate a lazy connect
        // so users without DATABASE_URL set can still scaffold.
        let needs_db = !matches!(
            args.first().map(String::as_str),
            Some("help") | Some("--help") | Some("-h")
                | Some("startapp") | Some("makemigrations")
                | Some("docs") | Some("version") | Some("--version")
                | Some("make:viewset") | Some("make:serializer")
                | Some("make:form") | Some("make:job")
                | Some("make:notification") | Some("make:middleware")
                | Some("make:test")
        );
        let pool = if needs_db {
            PgPool::connect(&url).await?
        } else {
            PgPool::connect_lazy(&url)?
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
        if let Some(seed) = self.seed {
            // Tenancy Builder's seed_with takes (Arc<TenantPools>, PgPool,
            // String); we forward the registry pool and discard the rest.
            builder = builder
                .seed_with(move |_pools, registry, _url| async move {
                    seed(&registry).await
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
        assert_eq!(cli.migrations_dir, std::path::PathBuf::from("custom/migrations"));
        assert!(cli.tenancy);
    }

    #[test]
    fn seed_hook_stored() {
        let cli = Cli::new().seed(|_pool| async { Ok(()) });
        assert!(cli.seed.is_some());
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
