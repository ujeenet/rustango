//! `rustango::server::Builder` — the runserver assembly.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use axum::{Extension, Router};
use sqlx::Database;
use tower::ServiceExt as _;

use crate::extractors::TenantContext;
use crate::tenancy::{
    admin::TenantAdminBuilder, operator_console, ChainResolver, DefaultTenantDb, HeaderResolver,
    ListenerPort, TenantPools,
};

/// Stateless API router that the user supplies. The Builder injects
/// `Extension<Arc<TenantContext>>` at serve time so [`crate::extractors::Tenant`]
/// works inside every handler.
pub type ApiRouter = Router<()>;

/// What every tenancy app's `main` builds before serving.
///
/// Generic over the backend (`DB = DefaultTenantDb` so existing PG
/// `Builder::from_env()` callers compile without a turbofish).
/// v0.38 — every internal handle (`registry`, `pools`) is per-backend;
/// sqlite + mysql tenancy apps get the same bundled operator-console +
/// tenant-admin shape by constructing via [`Builder::from_pool`].
pub struct Builder<DB: Database = DefaultTenantDb> {
    apex: String,
    registry_url: String,
    pools: Arc<TenantPools<DB>>,
    registry: sqlx::Pool<DB>,
    show_only: Vec<String>,
    admin_title: Option<String>,
    admin_subtitle: Option<String>,
    api: Option<ApiRouter>,
    admin_actions: Vec<PendingAction>,
    /// Bootstrap initializer used by [`Builder::migrate`]. Defaults
    /// to [`crate::tenancy::init_tenancy`]; swapped via
    /// [`Builder::user_model`] for a custom
    /// [`crate::tenancy::TenantUserModel`].
    init_tenancy_fn: crate::tenancy::manage::InitTenancyFn,
    /// v0.28.0 (#74) — configurable URL prefixes (login, admin,
    /// audit, static, brand) + session TTLs. Defaults to
    /// `RouteConfig::default()` (legacy `__`-prefixed paths).
    routes: crate::tenancy::RouteConfig,
    /// When `true`, the served Router gets `/health` + `/ready`
    /// endpoints merged in (using the registry pool for the
    /// `/ready` `SELECT 1` probe). Set via [`Builder::with_health`]
    /// so projects with custom health JSON can opt out.
    health_endpoints: bool,
    /// Migrations directory to hand a provisioner, set by
    /// [`Builder::with_tenant_provisioning`]. `None` means the
    /// operator console cannot create tenants — see that method for
    /// why this is opt-in rather than on by default.
    provisioning_dir: Option<std::path::PathBuf>,
    /// How long `serve` lets open connections finish after SIGTERM.
    drain_timeout: std::time::Duration,
    /// A `running` run silent this long is closed at boot.
    stale_run_after: std::time::Duration,
    /// Mounts registered via [`Builder::with_static_files`].
    /// Nested at `serve` time as `Router::nest(prefix, static_router(files))`
    /// before the admin fallback so they take precedence over the
    /// admin's catch-all.
    static_dirs: Vec<(String, crate::static_files::StaticFiles)>,
    /// Whether to mount request observability at all, set by
    /// [`Builder::observability`].
    ///
    /// It has to be applied here rather than by the caller. `Cli` used
    /// to layer its api router before handing it over, but this builder
    /// then merges the tenant admin into that router and dispatches the
    /// operator console on a sibling branch — and axum's own rule is
    /// that "routes added after `layer` is called will not have the
    /// middleware added". So the entire tenant-admin surface and the
    /// whole operator console served with no access log and no request
    /// span, on the multi-tenant path #1480 was opened about.
    ///
    /// Separate from `access_log` on purpose: `[logging] access_log =
    /// false` turns off the log, not the trace context.
    observability: bool,
    /// The access-log layer, when request logging is on. `None` with
    /// `observability == true` means "span and request id, no log line".
    access_log: Option<crate::access_log::AccessLogLayer>,
    /// Query params the span redacts, when the caller has named them.
    ///
    /// `None` means **derive from `access_log`**, which is what the
    /// span did before `span_redact` existed. Holding an `Option`
    /// rather than a list is what keeps the two in step: a default of
    /// `default_redact_params()` would silently drop a configured list
    /// whenever `observability()` was called without this setter,
    /// which is #1610 again with the access log *on*.
    span_redact: Option<Vec<String>>,
    /// Applied to the outermost router, so the tenant login, tenant
    /// admin and operator console carry them too (#1699).
    #[cfg(feature = "admin")]
    security_headers: Option<crate::security_headers::SecurityHeadersLayer>,
    /// Same reason: the Host allowlist and HTTPS redirect must see the
    /// tenant and console routes too (#1700).
    #[cfg(feature = "admin")]
    allowed_hosts: Option<crate::host_validation::AllowedHostsLayer>,
    #[cfg(feature = "admin")]
    ssl_redirect: Option<crate::ssl_redirect::SslRedirectLayer>,
    /// Outermost, so the access log and every throttle see its
    /// `TrustedRealIp` (#1745).
    #[cfg(feature = "admin")]
    real_ip: Option<crate::real_ip::RealIpLayer>,
    /// Opt-in `X-Org`-style fallback after the host resolvers (#1856).
    header_resolver: Option<HeaderResolver>,
    _phantom: PhantomData<DB>,
}

struct PendingAction {
    table: &'static str,
    name: &'static str,
    handler: crate::admin::AdminActionFn,
}

#[cfg(feature = "postgres")]
impl Builder<sqlx::Postgres> {
    /// Connect to `DATABASE_URL`, build [`TenantPools`], read
    /// `RUSTANGO_APEX_DOMAIN`. Tracing init is left to the caller —
    /// one `tracing_subscriber::fmt().init()` away.
    ///
    /// PG-only: defaults to `postgres://...` and uses
    /// `PgPool::connect`. For sqlite / mysql tenancy apps, use
    /// [`Builder::from_pool`] with the right `sqlx::Pool<DB>` instead.
    ///
    /// # Errors
    /// Connection to `DATABASE_URL` failures.
    pub async fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let apex = std::env::var("RUSTANGO_APEX_DOMAIN").unwrap_or_else(|_| "localhost".into());
        let registry_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://rustango:rustango@localhost:5432/rustango_test".into());
        let registry = crate::sql::Pool::connect_postgres(&registry_url).await?;
        Ok(Self::from_pool(registry, registry_url, apex))
    }
}

impl<DB: Database> Builder<DB> {
    /// Construct a Builder from an already-built `sqlx::Pool<DB>` and
    /// the registry URL string. Use this when you've configured the
    /// pool yourself (custom `PoolOptions`, after-connect hooks, etc.)
    /// or when you need a non-default backend (sqlite / mysql).
    ///
    /// `apex` is the apex domain for host-based dispatch; override
    /// via env-aware `Builder::from_env()` on PG, or wire your own
    /// value here.
    pub fn from_pool(
        registry: sqlx::Pool<DB>,
        registry_url: impl Into<String>,
        apex: impl Into<String>,
    ) -> Self {
        let pools = Arc::new(TenantPools::<DB>::new(registry.clone()));
        Self {
            apex: apex.into(),
            registry_url: registry_url.into(),
            pools,
            registry,
            show_only: Vec::new(),
            admin_title: None,
            admin_subtitle: None,
            api: None,
            admin_actions: Vec::new(),
            init_tenancy_fn: crate::tenancy::init_tenancy,
            routes: crate::tenancy::RouteConfig::default(),
            health_endpoints: false,
            provisioning_dir: None,
            drain_timeout: crate::shutdown::DEFAULT_DRAIN_TIMEOUT,
            stale_run_after: crate::tenancy::provision_store::STALE_RUN_AFTER,
            static_dirs: Vec::new(),
            observability: false,
            access_log: None,
            span_redact: None,
            #[cfg(feature = "admin")]
            security_headers: None,
            #[cfg(feature = "admin")]
            allowed_hosts: None,
            #[cfg(feature = "admin")]
            ssl_redirect: None,
            #[cfg(feature = "admin")]
            real_ip: None,
            header_resolver: None,
            _phantom: PhantomData,
        }
    }

    /// Send these security headers on every response, the tenant
    /// login, tenant admin and operator console included (#1699).
    /// `Cli` calls this for you from `[security]`.
    #[cfg(feature = "admin")]
    #[must_use]
    pub fn security_headers(
        mut self,
        layer: crate::security_headers::SecurityHeadersLayer,
    ) -> Self {
        self.security_headers = Some(layer);
        self
    }

    /// Refuse requests whose `Host` is not allowed, on every route
    /// (#1700). `Cli` calls this from `[security] allowed_hosts`.
    #[cfg(feature = "admin")]
    #[must_use]
    pub fn allowed_hosts(mut self, layer: crate::host_validation::AllowedHostsLayer) -> Self {
        self.allowed_hosts = Some(layer);
        self
    }

    /// Redirect plain HTTP to HTTPS on every route (#1700). `Cli`
    /// calls this from `[security] secure_ssl_redirect`.
    #[cfg(feature = "admin")]
    #[must_use]
    pub fn ssl_redirect(mut self, layer: crate::ssl_redirect::SslRedirectLayer) -> Self {
        self.ssl_redirect = Some(layer);
        self
    }

    /// Resolve the client IP from a trusted proxy on every route. A
    /// `RealIpLayer` on the api router runs after the access log reads it.
    #[cfg(feature = "admin")]
    #[must_use]
    pub fn real_ip(mut self, layer: crate::real_ip::RealIpLayer) -> Self {
        self.real_ip = Some(layer);
        self
    }

    /// Resolve the tenant from a request header when no host matched.
    ///
    /// Off by default, since the client picks the value (#1856). Pair
    /// it with [`HeaderResolver::allow_only`] or tenant-scoped credentials.
    #[must_use]
    pub fn header_resolver(mut self, resolver: HeaderResolver) -> Self {
        self.header_resolver = Some(resolver);
        self
    }

    /// Auto-mount `/health` (liveness) + `/ready` (readiness with
    /// `SELECT 1` against the registry pool) on the served
    /// router. Wired by [`crate::manage::Cli::with_health`] when
    /// tenancy mode is on; can also be called directly when
    /// constructing the server outside `Cli`.
    ///
    /// Default off — operators sometimes ship custom health JSON
    /// with additional checks (queue depth, Redis ping) and don't
    /// want the framework's defaults colliding.
    #[must_use]
    pub fn with_health(mut self) -> Self {
        self.health_endpoints = true;
        self
    }

    /// Log every request this server answers, on every serving branch.
    ///
    /// Applied to the **outermost** router in [`Self::into_router`], so
    /// the tenant app, the tenant admin merged into it, the operator
    /// console on the apex branch and the health endpoints all inherit
    /// it. Layering the api router before handing it here does not do
    /// that — the admin is merged in afterwards and the operator
    /// console is a sibling — which is how the multi-tenant path ended
    /// up with the two surfaces that most need attribution being the
    /// two that had none (#1480).
    ///
    /// `Cli` calls this for you from `[logging]`. A hand-built server also
    /// needs `security_headers`, `allowed_hosts` and `ssl_redirect`.
    #[must_use]
    pub fn observability(mut self, access_log: Option<crate::access_log::AccessLogLayer>) -> Self {
        self.observability = true;
        self.access_log = access_log;
        self
    }

    /// Override the query params the request span replaces with
    /// `[redacted]`.
    ///
    /// Optional. Without it the span uses the list from
    /// [`Self::observability`]'s access-log layer, and
    /// `default_redact_params()` when there is no layer — so a
    /// configured list reaches the span whether or not `[logging]
    /// access_log` is on (#1610). `Cli` calls this for you.
    ///
    /// Reach for it only when the span should redact something the
    /// access log does not.
    #[must_use]
    pub fn span_redact(mut self, params: Vec<String>) -> Self {
        self.span_redact = Some(params);
        self
    }

    /// Let operators **create** tenants from the console (#1322):
    /// `GET`/`POST /orgs/new`, the connection probe, and the
    /// provisioning run view + SSE stream.
    ///
    /// `migrations_dir` is where the new tenant's migrations come
    /// from — the same directory `migrate` uses, usually
    /// `"migrations"`.
    ///
    /// **Off by default, and deliberately.** Creating a tenant is the
    /// most dangerous thing the console can do: it takes a database
    /// URL and connects to it. Every authenticated operator who can
    /// reach the console can use these routes, because `Operator` has
    /// no permission model — so the meaningful boundary today is
    /// whether the deployment turns this on at all.
    #[must_use]
    pub fn with_tenant_provisioning(
        mut self,
        migrations_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.provisioning_dir = Some(migrations_dir.into());
        self
    }

    /// Auto-mount a [`crate::static_files::static_router`] at `prefix`
    /// serving files under `root_dir` on the tenant subdomain. Repeat
    /// to mount more than one directory. Wired by
    /// [`crate::manage::Cli::with_static`] when tenancy mode is on.
    #[must_use]
    pub fn with_static(
        self,
        prefix: impl Into<String>,
        root_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        let prefix = prefix.into();
        crate::static_files::warn_if_uploads_prefix(&prefix);
        let files = crate::static_files::StaticFiles::new(root_dir);
        self.with_static_files(prefix, files)
    }

    /// [`Self::with_static`] for files users uploaded: HTML, SVG and XML
    /// download instead of running on the tenant host
    /// ([`crate::static_files::StaticFiles::user_content`]).
    #[must_use]
    pub fn with_uploads(
        self,
        prefix: impl Into<String>,
        root_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        let files = crate::static_files::StaticFiles::new(root_dir).user_content();
        self.with_static_files(prefix, files)
    }

    /// Mount a configured [`crate::static_files::StaticFiles`] at `prefix`.
    #[must_use]
    pub fn with_static_files(
        mut self,
        prefix: impl Into<String>,
        files: crate::static_files::StaticFiles,
    ) -> Self {
        self.static_dirs.push((prefix.into(), files));
        self
    }

    /// Override the URL prefixes (login, admin, audit, static,
    /// brand) and session TTLs (#74, v0.28.0). Defaults to the
    /// legacy `__`-prefixed paths so upgrades are no-ops.
    /// Friendly preset:
    /// ```ignore
    /// .routes(rustango::tenancy::RouteConfig::friendly())
    /// ```
    /// Custom:
    /// ```ignore
    /// .routes(rustango::tenancy::RouteConfig {
    ///     login_url: "/sign-in".into(),
    ///     admin_url: "/manage".into(),
    ///     ..Default::default()
    /// })
    /// ```
    #[must_use]
    pub fn routes(mut self, routes: crate::tenancy::RouteConfig) -> Self {
        self.routes = routes;
        self
    }

    /// The tenant pools this builder will hand the server.
    ///
    /// Read access, mirroring [`TenantPools::pool_config`]. Useful for
    /// asserting that a [`Builder::tenant_pools`] call actually reached
    /// the pools — a setter that stores a value nothing reads is the
    /// shape of #1456, so being able to check is worth the method.
    #[must_use]
    pub fn pools(&self) -> &Arc<TenantPools<DB>> {
        &self.pools
    }

    /// Size the per-tenant connection pools (#1456).
    ///
    /// `from_pool` builds `TenantPools` with
    /// [`TenantPoolsConfig::default`](crate::tenancy::TenantPoolsConfig),
    /// and until this existed there was
    /// no way to change it: the type was public and documented, but
    /// every route to a running server went through a constructor that
    /// ignored it, and `tenancy/pools.rs` reads no environment
    /// variables. Connection counts multiply by tenant *and* by
    /// process — 20 database-mode tenants at the default 16 across a
    /// web and a worker process is 640 connections, against a stock
    /// `PostgreSQL` limit of 100 — so the only available lever was the
    /// database server's own `max_connections`, which is the wrong
    /// place to size an application's pools and often not the
    /// operator's to change.
    ///
    /// ```no_run
    /// # use rustango::tenancy::TenantPoolsConfig;
    /// # fn demo<DB: sqlx::Database>(b: rustango::server::Builder<DB>) -> rustango::server::Builder<DB> {
    /// b.tenant_pools(TenantPoolsConfig {
    ///     database_pool_max_connections: 4,
    ///     max_cached_database_pools: 200,
    ///     ..Default::default()
    /// })
    /// # }
    /// ```
    #[must_use]
    pub fn tenant_pools(mut self, config: crate::tenancy::TenantPoolsConfig) -> Self {
        // `TenantPools` is behind an `Arc` by this point, so rebuild
        // rather than mutate — the registry pool itself is cheap to
        // clone (it is an `Arc` internally).
        self.pools = Arc::new(TenantPools::<DB>::new(self.registry.clone()).config(config));
        self
    }

    /// Swap the tenant user model used by [`Builder::migrate`]. Same
    /// semantics as [`crate::manage::Cli::user_model`].
    #[must_use]
    pub fn user_model<U: crate::tenancy::TenantUserModel>(mut self) -> Self {
        self.init_tenancy_fn = crate::tenancy::init_tenancy_with::<U>;
        self
    }

    /// Set the display name shown in the tenant admin sidebar header.
    /// Defaults to `"Rustango Admin"` when not called.
    #[must_use]
    pub fn admin_title(mut self, title: impl Into<String>) -> Self {
        self.admin_title = Some(title.into());
        self
    }

    /// Set an optional subtitle shown below the admin title in the sidebar.
    #[must_use]
    pub fn admin_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.admin_subtitle = Some(subtitle.into());
        self
    }

    /// Limit the auto-mounted tenant admin to a subset of registered
    /// model tables. Same shape as
    /// [`TenantAdminBuilder::show_only`].
    #[must_use]
    pub fn admin_show_only<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.show_only = models.into_iter().map(Into::into).collect();
        self
    }

    /// Mount user-supplied API routes on the tenant subdomain. The
    /// router must be stateless ([`Router<()>`]); the
    /// [`crate::extractors::Tenant`] extractor reads from extensions,
    /// not state. Users with their own state can call
    /// `.with_state(...)` on their router before passing it here.
    #[must_use]
    pub fn api(mut self, router: ApiRouter) -> Self {
        self.api = Some(router);
        self
    }

    /// Register a user-defined bulk admin action that runs against the
    /// tenant pool of whichever tenant the request resolves to. The
    /// action name must also appear in the model's
    /// `#[rustango(admin(actions = "..."))]` allowlist.
    ///
    /// Mirrors [`crate::admin::Builder::register_action`]; the only
    /// difference is the handler receives the tenant-scoped pool.
    #[must_use]
    pub fn admin_register_action<F>(
        mut self,
        model_table: &'static str,
        action_name: &'static str,
        handler: F,
    ) -> Self
    where
        F: for<'a> Fn(
                &'a crate::sql::Pool,
                &'a [crate::core::SqlValue],
            ) -> crate::admin::AdminActionFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.admin_actions.push(PendingAction {
            table: model_table,
            name: action_name,
            handler: std::sync::Arc::new(handler),
        });
        self
    }

    /// Run a first-run hook with full access to pools + registry.
    /// Typical use: provision a sample tenant via
    /// `tenancy::manage::api::create_tenant_if_missing`, then seed
    /// rows via the ORM.
    ///
    /// # Errors
    /// Surfaces whatever the hook returns.
    ///
    /// The hook's error type is widened to `Box<dyn Error + Send +
    /// Sync>` (PR #606) so seed closures can hold non-`Send` errors
    /// across `.await` boundaries without losing future-Send-ness.
    /// The return type stays the bare `Box<dyn Error>` shape that
    /// callers propagate through `?` chains; the boundary coercion
    /// happens at the `.await?` point.
    pub async fn seed_with<F, Fut>(self, hook: F) -> Result<Self, Box<dyn std::error::Error>>
    where
        F: FnOnce(Arc<TenantPools<DB>>, sqlx::Pool<DB>, String) -> Fut,
        Fut: Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    {
        hook(
            self.pools.clone(),
            self.registry.clone(),
            self.registry_url.clone(),
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
        Ok(self)
    }

    /// Apply every migration discoverable from `project_root` to the
    /// registry + every active tenant. One call sets up a multi-app
    /// project:
    ///
    /// 1. Write the packaged tenancy bootstrap migrations
    ///    (`0001_rustango_registry_initial`, `0001_rustango_tenant_initial`)
    ///    into `<project_root>/migrations/` if they're not already
    ///    present — idempotent.
    /// 2. Discover every migrations directory under `project_root`:
    ///    the flat `<project_root>/migrations/` (project-level
    ///    bootstraps + project-root models) plus every
    ///    `<project_root>/<app>/migrations/` subdir scaffolded by
    ///    `manage startapp`.
    /// 3. For each discovered dir, apply registry-scoped migrations
    ///    against the registry pool, then tenant-scoped migrations
    ///    against every active org's storage. Per-tenant isolation:
    ///    failures on one tenant don't abort the others.
    ///
    /// Back-compat with v0.8.1: if `project_root` does **not** contain
    /// a `migrations/` subdir but DOES itself contain `*.json`
    /// migration files, it's treated as the migrations dir directly
    /// (the v0.8.1 single-dir shape). Pass the project root for
    /// multi-app discovery; pass the flat `migrations/` dir for the
    /// pre-9.0g flat layout.
    ///
    /// # Errors
    /// I/O failures creating directories or writing bootstrap files;
    /// [`crate::tenancy::TenancyError`] from the registry or tenant
    /// migration runners.
    pub async fn migrate<P: AsRef<std::path::Path>>(
        self,
        project_root: P,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        crate::sql::Pool: From<sqlx::Pool<DB>>,
    {
        let root = project_root.as_ref();
        std::fs::create_dir_all(root)?;

        // Detect which shape the user passed. If `<root>/migrations/`
        // exists OR `<root>/<app>/migrations/` exists, we're a project
        // root. Otherwise (root contains *.json directly), back-compat
        // single-dir mode.
        let dirs = crate::migrate::discover_migration_dirs(root);
        if dirs.is_empty() && root_has_json_files(root) {
            // v0.8.1 shape: user passed the flat migrations dir.
            (self.init_tenancy_fn)(root)?;
            let _ = crate::tenancy::migrate_registry(self.pools.as_ref(), root).await?;
            let _ =
                crate::tenancy::migrate_tenants_dyn(self.pools.as_ref(), root, &self.registry_url)
                    .await?;
            return Ok(self);
        }

        // 9.0g shape: walk every per-app dir + the flat dir.
        let flat = root.join("migrations");
        std::fs::create_dir_all(&flat)?;
        (self.init_tenancy_fn)(&flat)?;

        // Re-discover after init_tenancy populated the flat dir.
        let dirs = crate::migrate::discover_migration_dirs(root);
        for dir in &dirs {
            let _ = crate::tenancy::migrate_registry(self.pools.as_ref(), dir).await?;
            let _ =
                crate::tenancy::migrate_tenants_dyn(self.pools.as_ref(), dir, &self.registry_url)
                    .await?;
        }
        Ok(self)
    }

    /// Bind + serve. Owns the host dispatcher, operator console,
    /// tenant admin, and the API router fallback.
    ///
    /// # Errors
    /// `bind` failure, or the underlying `axum::serve` call
    /// returning an error.
    /// Assemble the whole application — operator console, tenant
    /// admin, the user's API, host dispatch — and hand back the
    /// router, without binding a socket.
    ///
    /// Split out of [`Self::serve`] so the assembly is *testable*.
    /// It was not, and that cost: `with_tenant_provisioning` was
    /// added, the console constructor existed, and nothing connected
    /// them — invisible to every test, because the only way to
    /// exercise this code was to start a real server. A builder whose
    /// output can only be observed by binding a port is a builder
    /// nobody checks.
    ///
    /// # Errors
    /// Pre-warm and bootstrap failures surfaced during assembly.
    pub async fn into_router(self) -> Result<Router, Box<dyn std::error::Error>>
    where
        crate::sql::Pool: From<sqlx::Pool<DB>>,
    {
        let resolver_for_admin = self.resolver();

        // v0.27.7 (#60) — pre-warm tenant pools on boot when the
        // app's `TenantPoolsConfig.prewarm_active_tenants` flag
        // is on. Default is false so existing apps don't take
        // a longer boot time on upgrade. Failures are logged per
        // tenant but don't abort `serve` — the lazy hot-path
        // build will retry on the first request.
        if self.pools.pool_config().prewarm_active_tenants {
            match self.pools.prewarm_database_tenants().await {
                Ok(report) => {
                    tracing::info!(
                        target: "rustango::server",
                        warmed = report.warmed,
                        failed = report.failed,
                        skipped_cap = report.skipped_cap,
                        "tenant pools pre-warmed at boot",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "rustango::server",
                        error = %e,
                        "tenant-pool pre-warm failed (non-fatal; lazy build will retry)",
                    );
                }
            }
        }

        // v0.27.2 — persist generated secrets to disk so dev
        // `cargo run` cycles don't sign every operator out on
        // restart (#69). Production should still set
        // `RUSTANGO_SESSION_SECRET`. Two distinct paths so a
        // tenant cookie and an operator cookie can't be confused.
        // Audit M2 — prod tier requires a valid RUSTANGO_SESSION_SECRET
        // and fails closed; dev/staging keep the disk-persisted keys.
        let tier = crate::session::tier_from_env();
        let session_secret_for_tenant = crate::session::load_session_secret_for_tier(
            &tier,
            std::path::Path::new("./var/.rustango_tenant_session.key"),
        );
        let operator_secret = crate::session::load_session_secret_for_tier(
            &tier,
            std::path::Path::new("./var/.rustango_operator_session.key"),
        );
        let ctx = Arc::new(TenantContext {
            pools: self.pools.clone(),
            resolver: self.resolver(),
            session_secret: session_secret_for_tenant.clone(),
            operator_secret: operator_secret.clone(),
        });
        let mut tenant_admin_builder = TenantAdminBuilder::new(
            self.pools.clone(),
            self.registry_url.clone(),
            resolver_for_admin,
        )
        .routes(self.routes.clone());
        if !self.show_only.is_empty() {
            tenant_admin_builder = tenant_admin_builder.show_only(self.show_only.clone());
        }
        if let Some(t) = self.admin_title {
            tenant_admin_builder = tenant_admin_builder.title(t);
        }
        if let Some(s) = self.admin_subtitle {
            tenant_admin_builder = tenant_admin_builder.subtitle(s);
        }
        for action in self.admin_actions {
            let handler = action.handler;
            tenant_admin_builder = tenant_admin_builder.register_action(
                action.table,
                action.name,
                move |pool, pks| handler(pool, pks),
            );
        }
        let tenant_admin = tenant_admin_builder
            .with_session(session_secret_for_tenant.clone())
            .build();

        // Optionally merge health endpoints onto the user's API
        // router before we layer the admin fallback. Uses the
        // registry pool for the `/ready` SELECT 1 probe — that's
        // the right scope for tenancy projects (registry health
        // gates traffic to every tenant).
        //
        // Static-dir mounts happen on the same router so they take
        // precedence over the admin fallback for paths under their
        // prefix. If `self.api` is `None` we synthesize an empty
        // router so static / health mounts still work without a
        // user-supplied API.
        let had_api = self.api.is_some();
        let api = if had_api || self.health_endpoints || !self.static_dirs.is_empty() {
            let mut r = self.api.unwrap_or_default();
            for (prefix, files) in &self.static_dirs {
                r = r.nest(prefix, crate::static_files::static_router(files.clone()));
            }
            if self.health_endpoints {
                r = r.merge(crate::health::health_router(self.registry.clone()));
            }
            Some(r)
        } else {
            None
        };

        // Build a Router that claims every path tenant_admin owns —
        // admin proper at `routes.admin_url/*`, plus the auth-,
        // static-, and brand-surface paths that live outside the
        // admin tree. The legacy `/__admin*` paths are also kept
        // for back-compat with apps still on `RouteConfig::legacy()`
        // or hard-coded links.
        //
        // Crucially we do NOT attach `tenant_admin` as a fallback
        // here. That used to mean "the admin owns every unmatched
        // URL", which clobbered any `.fallback()` set inside the
        // user's API router (axum semantics) — most visibly:
        // `/` on a CMS tenant rendered the admin index instead of
        // the public CMS home, and `/<slug>` rendered the admin's
        // `/{table}` catch-all ("table not found"). With explicit
        // routes, the user's API router fallback is free to take
        // every URL the admin doesn't claim.
        let admin_routes = build_admin_routes(&tenant_admin, &self.routes);
        let tenant_app = match api {
            Some(router) => router.layer(Extension(ctx.clone())).merge(admin_routes),
            None => admin_routes,
        };

        // `router_with_pools` (rather than `router`) so the operator
        // console exposes /orgs/{slug}/edit. The pool handle is needed
        // because rotating `database_url` must evict the cached
        // `TenantPool` for that org so the next request rebuilds with
        // new credentials — without eviction the operator could
        // change the URL in the DB and the cached pool would happily
        // keep authenticating with stale creds until process restart.
        // v0.27.8 (#78) wired the operator console's
        // `/orgs/{slug}/impersonate` flow; v0.29 (#88) flipped it
        // from a cookie-domain handoff to a URL-token handoff so
        // it works on Chromium against the `localhost` PSL TLD
        // (where `Domain=.localhost` cookies are silently
        // rejected on subdomains). The operator console now mints
        // a signed token, redirects to
        // `<sub>.<apex><handoff_url>?token=<...>`, and the tenant
        // admin redeems the token + sets a host-scoped cookie. No
        // cookie is set on the operator-console origin.
        let brand_storage_for_op = crate::tenancy::branding::default_brand_storage();
        // #1322 — only when the deployment asked for it. See
        // `with_tenant_provisioning` for why creating tenants is not
        // on by default.
        // Runs are detached tasks; close the ones a stopped process left `running` (#1883).
        if self.provisioning_dir.is_some() {
            let registry: crate::sql::Pool = self.registry.clone().into();
            let after = self.stale_run_after;
            match crate::tenancy::provision_store::reap_stale_runs(&registry, after).await {
                Ok(0) => {}
                Ok(n) => tracing::warn!(
                    target: "rustango::tenancy::provision",
                    closed = n,
                    "closed provisioning/migration runs left running by a stopped process",
                ),
                Err(e) => tracing::warn!(
                    target: "rustango::tenancy::provision",
                    error = %e,
                    "could not check for interrupted provisioning runs",
                ),
            }
        }
        let provisioner = self.provisioning_dir.map(|dir| {
            crate::tenancy::provision::Provisioner::new(
                self.pools.clone(),
                self.registry_url.clone(),
                dir,
            )
            .erased()
        });
        let operator_admin = operator_console::router_full_unlogged(
            self.registry.into(),
            Some(self.pools.clone().into_invalidator()),
            provisioner,
            operator_secret,
            brand_storage_for_op,
            Some(session_secret_for_tenant.clone()),
            // Handoff URL on the tenant admin where the token
            // gets redeemed (#88). RouteConfig holds the canonical
            // value; default `/_impersonation_handoff`. After
            // redemption the tenant admin reads its own
            // `routes.admin_url` to build the final redirect
            // target — no need to thread it from the operator
            // console.
            self.routes.impersonation_handoff_url.clone(),
        );
        // With an outer access log, it logs the console (#1788).
        let operator_admin = if self.access_log.is_some() {
            operator_admin
        } else {
            use crate::access_log::AccessLogRouterExt as _;
            operator_admin.access_log(operator_console::access_log_layer())
        };

        let app = Router::new().fallback_service(tower::service_fn({
            let operator = operator_admin.clone();
            let tenants = tenant_app.clone();
            let apex = self.apex.clone();
            move |req: axum::http::Request<axum::body::Body>| {
                let mut operator = operator.clone();
                let mut tenants = tenants.clone();
                let apex = apex.clone();
                async move {
                    let on_apex = crate::tenancy::host_is_apex(req.headers(), req.uri(), &apex);
                    let response = if on_apex {
                        operator.as_service().oneshot(req).await
                    } else {
                        tenants.as_service().oneshot(req).await
                    };
                    response.map_err(|e| -> std::convert::Infallible {
                        panic!("axum router service is Infallible: {e}")
                    })
                }
            }
        }));

        // Inside the security headers, so a panic's 500 carries them (#1541).
        let app = crate::panic_guard::catch_panics(app);
        #[cfg(feature = "admin")]
        let app = match self.security_headers {
            Some(layer) => {
                use crate::security_headers::SecurityHeadersRouterExt as _;
                app.security_headers(layer)
            }
            None => app,
        };
        // Same order as the single-tenant router: the Host allowlist is
        // outermost, so a bad Host is refused, never redirected to.
        #[cfg(feature = "admin")]
        let app = match self.ssl_redirect {
            Some(layer) => {
                use crate::ssl_redirect::SslRedirectRouterExt as _;
                app.ssl_redirect(layer)
            }
            None => app,
        };
        #[cfg(feature = "admin")]
        let app = match self.allowed_hosts {
            Some(layer) => {
                use crate::host_validation::AllowedHostsRouterExt as _;
                app.allowed_hosts(layer)
            }
            None => app,
        };

        // Observability goes on the OUTERMOST router, after both
        // branches are behind the Host dispatch, so tenant app, tenant
        // admin, operator console and the health endpoints all carry
        // it. Anything layered on the api router before it reached this
        // builder covered only the api router (#1480).
        let app = if self.observability {
            // Resolved here rather than at each setter, so the order
            // `observability()` and `span_redact()` are called in
            // cannot change the result and neither can clobber the
            // other.
            // It now logs the console too, whose `?next=` holds the attempted URL.
            let access_log = self.access_log.map(|l| l.redact_additional("next"));
            let mut redact = resolve_span_redact(self.span_redact.clone(), access_log.as_ref());
            // The span covers the console in every case, access log or not.
            if !redact.iter().any(|p| p == "next") {
                redact.push("next".to_owned());
            }
            // One definition, shared with `Cli::mount_observability` —
            // see `access_log::mount_observability` for the ordering
            // rules and why they live in one place.
            crate::access_log::mount_observability(app, access_log, redact)
        } else {
            app
        };
        #[cfg(feature = "admin")]
        let app = match self.real_ip {
            Some(layer) => {
                use crate::real_ip::RealIpRouterExt as _;
                app.real_ip(layer)
            }
            None => app,
        };

        Ok(app)
    }

    /// The standard chain, plus the opt-in header fallback.
    pub(crate) fn resolver(&self) -> ChainResolver {
        let chain = ChainResolver::standard(self.apex.clone());
        match &self.header_resolver {
            Some(h) => chain.push(h.clone()),
            None => chain,
        }
    }

    /// How long [`Self::serve`] lets open connections finish after the
    /// stop signal. Default [`crate::shutdown::DEFAULT_DRAIN_TIMEOUT`].
    #[must_use]
    pub fn drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// A provisioning/migration run `running` with no event for this long
    /// is closed as failed at boot. Default
    /// [`crate::tenancy::provision_store::STALE_RUN_AFTER`] (1 h); keep it
    /// above your slowest silent step.
    #[must_use]
    pub fn stale_run_after(mut self, after: std::time::Duration) -> Self {
        self.stale_run_after = after;
        self
    }

    /// Assemble everything and bind.
    ///
    /// # Errors
    /// As [`Self::into_router`], plus a bind failure on `addr`.
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>>
    where
        crate::sql::Pool: From<sqlx::Pool<DB>>,
    {
        let drain = self.drain_timeout;
        let app = self.into_router().await?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let app = tag_listener_port(app, &listener)?;
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
        Ok(())
    }
}

/// `PortResolver` reads this, never the client-sent URI port (#1856).
fn tag_listener_port(app: Router, listener: &tokio::net::TcpListener) -> std::io::Result<Router> {
    Ok(app.layer(Extension(ListenerPort(listener.local_addr()?.port()))))
}

/// Build the axum router that claims every URL the tenant admin
/// is responsible for — admin proper under `routes.admin_url`, plus
/// the auth/static/brand surface that has to live at the top level.
///
/// All routes forward to a wrapper around the same `tenant_admin`
/// service. The service's `handle_request` does its own path-based
/// dispatch (login form vs. admin index vs. brand static), so the
/// outer axum router just needs to enumerate every path it should
/// claim. Everything else falls through to the user's API router.
fn build_admin_routes(tenant_admin: &Router, routes: &crate::tenancy::RouteConfig) -> Router {
    use axum::routing::any;

    // Each `.route` call consumes its handler — `make` returns a
    // fresh closure-handler each time. The inner service is cheap
    // to clone (just an Arc-of-router under the hood).
    let make = || {
        let svc = tenant_admin.clone();
        move |req: axum::http::Request<axum::body::Body>| {
            let svc = svc.clone();
            async move {
                // A fresh request drops the outer router's path params,
                // which the inner `Path` extractors would otherwise see.
                let (parts, body) = req.into_parts();
                let mut builder = axum::http::Request::builder()
                    .method(&parts.method)
                    .uri(&parts.uri);
                for (k, v) in &parts.headers {
                    builder = builder.header(k, v);
                }
                let mut fresh = builder.body(body).expect("valid request");
                // Keep the client IP the login limits key on.
                let ext = fresh.extensions_mut();
                if let Some(ci) = parts
                    .extensions
                    .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                {
                    ext.insert(*ci);
                }
                #[cfg(feature = "admin")]
                if let Some(ip) = parts.extensions.get::<crate::real_ip::TrustedRealIp>() {
                    ext.insert(*ip);
                }
                svc.oneshot(fresh)
                    .await
                    .unwrap_or_else(|_| unreachable!("Router is Infallible"))
            }
        }
    };

    let admin_slash = format!("{}/", routes.admin_url);
    let admin_glob = format!("{}/{{*rest}}", routes.admin_url);
    let static_glob = format!("{}/{{*rest}}", routes.static_url);
    let brand_glob = format!("{}/{{*rest}}", routes.brand_url);

    let mut r = Router::new()
        // Admin proper.
        .route(&routes.admin_url, any(make()))
        .route(&admin_slash, any(make()))
        .route(&admin_glob, any(make()))
        // Auth / session surface (lives outside admin_url).
        .route(&routes.login_url, any(make()))
        .route(&routes.logout_url, any(make()))
        .route(&routes.change_password_url, any(make()))
        .route(&routes.impersonation_handoff_url, any(make()))
        // Static + brand assets.
        .route(&static_glob, any(make()))
        .route(&brand_glob, any(make()))
        // End-impersonation has a hard-coded fallback inside
        // `handle_request` for direct API callers.
        .route("/__end-impersonation", any(make()));

    // admin-sso: the OAuth begin + callback live at `{login}/sso` and
    // `{login}/sso/callback`. They're only handled inside `tenant_admin`'s
    // fallback dispatch, so without explicit mounts here an app that ships a
    // page catch-all (e.g. rustango-cms's `PublicRouter`) shadows the GET and
    // the button 404s. Claim them explicitly — like `login_url` above — so they
    // take precedence over the app router's fallback and reach the dispatch.
    #[cfg(feature = "admin-sso")]
    {
        // Multi-provider: `{login}/sso/{slug}` + `{login}/sso/{slug}/callback`.
        // A single wildcard mount claims the whole subtree so an app's page
        // catch-all can't shadow any per-provider route.
        let sso_glob = format!("{}/sso/{{*rest}}", routes.login_url);
        r = r.route(&sso_glob, any(make()));
    }

    // Legacy `/__admin*` mounts kept for back-compat with apps still
    // on `RouteConfig::legacy()` or hard-coded URLs. Skip when the
    // configured admin_url IS `/__admin` (would collide with the
    // routes above).
    if routes.admin_url != "/__admin" {
        r = r
            .route("/__admin", any(make()))
            .route("/__admin/", any(make()))
            .route("/__admin/{*rest}", any(make()));
    }

    r
}

/// Whether `root` contains any `*.json` files at the top level. Used
/// to detect the v0.8.1 single-dir shape of `Builder::migrate(dir)`
/// for back-compat — if a user passed the migrations dir itself,
/// rather than the project root, `discover_migration_dirs` finds
/// nothing but the dir clearly has migration files.
fn root_has_json_files(root: &std::path::Path) -> bool {
    let Ok(read) = std::fs::read_dir(root) else {
        return false;
    };
    read.filter_map(Result::ok)
        .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
}

/// The query params the request span redacts.
///
/// `explicit` is [`Builder::span_redact`]; `None` derives the list
/// from the access-log layer, and falls back to the defaults when
/// there is no layer.
///
/// A free function so the rule can be tested without a registry pool,
/// and so the precedence lives in one place rather than in whichever
/// setter ran last.
fn resolve_span_redact(
    explicit: Option<Vec<String>>,
    access_log: Option<&crate::access_log::AccessLogLayer>,
) -> Vec<String> {
    explicit
        .or_else(|| access_log.map(|l| l.redact_query_params.clone()))
        .unwrap_or_else(crate::access_log::default_redact_params)
}

#[cfg(all(test, feature = "sqlite"))]
pub(crate) mod resolver_tests {
    use super::*;
    use crate::tenancy::{Org, OrgResolver as _};

    /// A SQLite registry holding org `acme`, and its URL.
    pub(crate) async fn registry() -> (tempfile::TempDir, sqlx::SqlitePool, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let sq = sqlx::SqlitePool::connect(&url).await.expect("connect");
        let pool = crate::sql::Pool::Sqlite(sq.clone());
        crate::testkit::create_tables_for::<Org>(&pool)
            .await
            .expect("orgs");
        let mut org = Org {
            slug: "acme".into(),
            display_name: "acme".into(),
            backend_kind: "sqlite".into(),
            ..crate::testkit::org()
        };
        org.insert_pool(&pool).await.expect("insert org");
        (tmp, sq, url)
    }

    /// The slug the builder's chain picks for `X-Org: acme` on an unknown host.
    pub(crate) async fn x_org_pick(
        b: &Builder<sqlx::Sqlite>,
        sq: &sqlx::SqlitePool,
    ) -> Option<String> {
        let (parts, ()) = axum::http::Request::builder()
            .uri("/")
            .header("host", "shared.localhost")
            .header("x-org", "acme")
            .body(())
            .unwrap()
            .into_parts();
        let pool = crate::sql::Pool::Sqlite(sq.clone());
        b.resolver()
            .resolve(&parts, &pool)
            .await
            .expect("resolve")
            .map(|o| o.slug)
    }

    /// #1856 — without `.header_resolver()` a client `X-Org` picks no tenant.
    #[tokio::test]
    async fn x_org_is_ignored_unless_opted_in() {
        let _iso = crate::tenancy::isolated_resolver().await;
        let (_tmp, sq, url) = registry().await;
        let plain = Builder::<sqlx::Sqlite>::from_pool(sq.clone(), url.clone(), "localhost");
        assert_eq!(x_org_pick(&plain, &sq).await, None);
        let opted = Builder::<sqlx::Sqlite>::from_pool(sq.clone(), url, "localhost")
            .header_resolver(HeaderResolver::default());
        assert_eq!(x_org_pick(&opted, &sq).await.as_deref(), Some("acme"));
    }

    /// #1856 — handlers see the port the listener accepted on.
    #[tokio::test]
    async fn served_router_carries_the_listener_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new().route(
            "/",
            axum::routing::get(|p: Option<Extension<ListenerPort>>| async move {
                p.map_or(String::new(), |Extension(ListenerPort(n))| n.to_string())
            }),
        );
        let app = tag_listener_port(app, &listener).unwrap();
        let resp = app
            .oneshot(axum::http::Request::new(axum::body::Body::empty()))
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        let want = listener.local_addr().unwrap().port().to_string();
        assert_eq!(&body[..], want.as_bytes());
    }
}

#[cfg(test)]
mod span_redact_tests {
    use super::resolve_span_redact;
    use crate::access_log::{default_redact_params, AccessLogLayer};

    fn layer_with(name: &str) -> AccessLogLayer {
        let mut l = AccessLogLayer::default();
        l.redact_query_params.push(name.to_owned());
        l
    }

    /// With no explicit list, the span takes the access log's — which
    /// is what the span did before `span_redact` existed.
    ///
    /// Defaulting the field to `default_redact_params()` instead made
    /// a hand-built `Builder` drop a configured name unless the caller
    /// also remembered the setter: redacted in the access-log event
    /// and in clear text on the span, same request (#1610).
    #[test]
    fn without_an_explicit_list_the_span_follows_the_access_log() {
        let got = resolve_span_redact(None, Some(&layer_with("invite_token")));
        assert!(
            got.iter().any(|p| p == "invite_token"),
            "the configured name must reach the span: {got:?}"
        );
        assert!(
            got.iter().any(|p| p == "password"),
            "and the defaults it extends must survive: {got:?}"
        );
    }

    /// No layer at all — `[logging] access_log = false` — still gets
    /// the defaults rather than an empty list.
    #[test]
    fn with_no_access_log_the_span_gets_the_defaults() {
        assert_eq!(resolve_span_redact(None, None), default_redact_params());
    }

    /// An explicit list wins over the layer, so the span can redact
    /// something the access log does not.
    #[test]
    fn an_explicit_list_overrides_the_access_log() {
        let got = resolve_span_redact(
            Some(vec!["only_this".to_owned()]),
            Some(&layer_with("invite_token")),
        );
        assert_eq!(got, vec!["only_this".to_owned()]);
    }
}
