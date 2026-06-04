//! `rustango::server::Builder` — the runserver assembly.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use axum::{Extension, Router};
use sqlx::Database;
use tower::ServiceExt as _;

use crate::extractors::TenantContext;
#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;
use crate::tenancy::{
    admin::TenantAdminBuilder,
    operator_console::{self, SessionSecret},
    ChainResolver, DefaultTenantDb, HeaderResolver, SubdomainResolver, TenantPools,
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
    /// `(prefix, root_dir)` pairs registered via [`Builder::with_static`].
    /// Mounted at `serve` time as
    /// `Router::nest(prefix, static_router(StaticFiles::new(root_dir)))`
    /// before the admin fallback so they take precedence over the
    /// admin's catch-all.
    static_dirs: Vec<(String, std::path::PathBuf)>,
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
        let registry = PgPool::connect(&registry_url).await?;
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
            static_dirs: Vec::new(),
            _phantom: PhantomData,
        }
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

    /// Auto-mount a [`crate::static_files::static_router`] at `prefix`
    /// serving files under `root_dir` on the tenant subdomain. Repeat
    /// to mount more than one directory. Wired by
    /// [`crate::manage::Cli::with_static`] when tenancy mode is on.
    #[must_use]
    pub fn with_static(
        mut self,
        prefix: impl Into<String>,
        root_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.static_dirs.push((prefix.into(), root_dir.into()));
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
    /// registry + every active tenant. The Django-shape one-call setup
    /// for multi-app projects:
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
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>>
    where
        crate::sql::Pool: From<sqlx::Pool<DB>>,
    {
        let resolver_for_admin = build_resolver(&self.apex);

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
        let session_secret_for_tenant = SessionSecret::from_env_or_disk(std::path::Path::new(
            "./var/.rustango_tenant_session.key",
        ));
        let operator_secret = SessionSecret::from_env_or_disk(std::path::Path::new(
            "./var/.rustango_operator_session.key",
        ));
        let ctx = Arc::new(TenantContext {
            pools: self.pools.clone(),
            resolver: build_resolver(&self.apex),
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
            for (prefix, root) in &self.static_dirs {
                r = r.nest(
                    prefix,
                    crate::static_files::static_router(crate::static_files::StaticFiles::new(
                        root.clone(),
                    )),
                );
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
        let operator_admin = operator_console::router_with_impersonation(
            self.registry,
            self.pools.clone().into_invalidator(),
            operator_secret,
            brand_storage_for_op,
            session_secret_for_tenant.clone(),
            // Handoff URL on the tenant admin where the token
            // gets redeemed (#88). RouteConfig holds the canonical
            // value; default `/_impersonation_handoff`. After
            // redemption the tenant admin reads its own
            // `routes.admin_url` to build the final redirect
            // target — no need to thread it from the operator
            // console.
            self.routes.impersonation_handoff_url.clone(),
        );

        let app = Router::new().fallback_service(tower::service_fn({
            let operator = operator_admin.clone();
            let tenants = tenant_app.clone();
            let apex = self.apex.clone();
            move |req: axum::http::Request<axum::body::Body>| {
                let mut operator = operator.clone();
                let mut tenants = tenants.clone();
                let apex = apex.clone();
                async move {
                    let host = req
                        .headers()
                        .get(axum::http::header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.split(':').next().unwrap_or(s).to_owned())
                        .unwrap_or_default();
                    let response = if host == apex {
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

        let listener = tokio::net::TcpListener::bind(addr).await?;
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
    }
}

fn build_resolver(apex: &str) -> ChainResolver {
    ChainResolver::new()
        .push(SubdomainResolver::new(apex.to_owned()))
        .push(HeaderResolver::default())
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
                let (parts, body) = req.into_parts();
                let mut builder = axum::http::Request::builder()
                    .method(&parts.method)
                    .uri(&parts.uri);
                for (k, v) in &parts.headers {
                    builder = builder.header(k, v);
                }
                let fresh = builder.body(body).expect("valid request");
                svc.clone()
                    .oneshot(fresh)
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
