//! `rustango::server::Builder` — the runserver assembly.

use std::future::Future;
use std::sync::Arc;

use axum::{Extension, Router};
use tower::ServiceExt as _;

use crate::extractors::TenantContext;
use crate::sql::sqlx::PgPool;
use crate::tenancy::{
    admin::TenantAdminBuilder,
    operator_console::{self, SessionSecret},
    ChainResolver, HeaderResolver, SubdomainResolver, TenantPools,
};

/// Stateless API router that the user supplies. The Builder injects
/// `Extension<Arc<TenantContext>>` at serve time so [`crate::extractors::Tenant`]
/// works inside every handler.
pub type ApiRouter = Router<()>;

/// What every tenancy app's `main` builds before serving.
pub struct Builder {
    apex: String,
    registry_url: String,
    pools: Arc<TenantPools>,
    registry: PgPool,
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
}

struct PendingAction {
    table: &'static str,
    name: &'static str,
    handler: crate::admin::AdminActionFn,
}

impl Builder {
    /// Connect to `DATABASE_URL`, build [`TenantPools`], read
    /// `RUSTANGO_APEX_DOMAIN`. Tracing init is left to the caller —
    /// one `tracing_subscriber::fmt().init()` away.
    ///
    /// # Errors
    /// Connection to `DATABASE_URL` failures.
    pub async fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let apex = std::env::var("RUSTANGO_APEX_DOMAIN").unwrap_or_else(|_| "localhost".into());
        let registry_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgres://rustango:rustango@localhost:5432/rustango_test".into()
        });
        let registry = PgPool::connect(&registry_url).await?;
        let pools = Arc::new(TenantPools::new(registry.clone()));
        Ok(Self {
            apex,
            registry_url,
            pools,
            registry,
            show_only: Vec::new(),
            admin_title: None,
            admin_subtitle: None,
            api: None,
            admin_actions: Vec::new(),
            init_tenancy_fn: crate::tenancy::init_tenancy,
        })
    }

    /// Swap the tenant user model used by [`Builder::migrate`]. Same
    /// semantics as [`crate::manage::Cli::user_model`].
    #[must_use]
    pub fn user_model<U: crate::tenancy::TenantUserModel>(mut self) -> Self {
        self.init_tenancy_fn = crate::tenancy::init_tenancy_with::<U>;
        self
    }

    /// Set the display name shown in the tenant admin sidebar header.
    /// Defaults to `"rustango admin"` when not called.
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
                &'a crate::sql::sqlx::PgPool,
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
    pub async fn seed_with<F, Fut>(self, hook: F) -> Result<Self, Box<dyn std::error::Error>>
    where
        F: FnOnce(Arc<TenantPools>, PgPool, String) -> Fut,
        Fut: Future<Output = Result<(), Box<dyn std::error::Error>>>,
    {
        hook(self.pools.clone(), self.registry.clone(), self.registry_url.clone()).await?;
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
    ) -> Result<Self, Box<dyn std::error::Error>> {
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
            let _ = crate::tenancy::migrate_registry(&self.pools, root).await?;
            let _ = crate::tenancy::migrate_tenants(&self.pools, root, &self.registry_url).await?;
            return Ok(self);
        }

        // 9.0g shape: walk every per-app dir + the flat dir.
        let flat = root.join("migrations");
        std::fs::create_dir_all(&flat)?;
        (self.init_tenancy_fn)(&flat)?;

        // Re-discover after init_tenancy populated the flat dir.
        let dirs = crate::migrate::discover_migration_dirs(root);
        for dir in &dirs {
            let _ = crate::tenancy::migrate_registry(&self.pools, dir).await?;
            let _ = crate::tenancy::migrate_tenants(&self.pools, dir, &self.registry_url).await?;
        }
        Ok(self)
    }

    /// Bind + serve. Owns the host dispatcher, operator console,
    /// tenant admin, and the API router fallback.
    ///
    /// # Errors
    /// `bind` failure, or the underlying `axum::serve` call
    /// returning an error.
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let resolver_for_admin = build_resolver(&self.apex);

        let session_secret_for_tenant = SessionSecret::from_env_or_random();
        let operator_secret = SessionSecret::from_env_or_random();
        let ctx = Arc::new(TenantContext {
            pools: self.pools.clone(),
            resolver: build_resolver(&self.apex),
            session_secret: session_secret_for_tenant.clone(),
            operator_secret: operator_secret.clone(),
            registry: self.registry.clone(),
        });
        let mut tenant_admin_builder = TenantAdminBuilder::new(
            self.pools.clone(),
            self.registry_url.clone(),
            resolver_for_admin,
        );
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
            .with_session(session_secret_for_tenant)
            .build();

        // Admin CRUD lives under `/__admin/*` registered as explicit
        // routes so they take priority over user routes like `/post/{slug}`
        // whose typed Path extractor would otherwise capture `/__admin/post`
        // before the fallback. Session routes (`/__login`, `/__logout`,
        // `/__static__`) remain as fallback-only paths.
        //
        // We pass the full Request (including the `/__admin` prefix in the
        // URI) to the tenant admin; `handle_request` strips the prefix before
        // dispatching to the inner admin router so that redirect `next=` params
        // correctly reference `/__admin/...` paths.
        // Build a fresh request (method + URI + headers only, no outer-router
        // path-param extensions) before forwarding to the admin service.
        // Without this, axum's path extractor sees params from the outer
        // `/__admin/{*rest}` match stacked on top of the inner admin router's
        // own params, producing "Wrong number of path arguments" errors.
        let make_admin_handler =
            |svc: Router| {
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
                        svc.oneshot(fresh)
                            .await
                            .unwrap_or_else(|_| unreachable!("Router is Infallible"))
                    }
                }
            };
        let tenant_app = match self.api {
            Some(router) => {
                let h1 = make_admin_handler(tenant_admin.clone());
                let h2 = make_admin_handler(tenant_admin.clone());
                let h3 = make_admin_handler(tenant_admin.clone());
                router
                    .layer(Extension(ctx.clone()))
                    .route("/__admin", axum::routing::any(h1))
                    .route("/__admin/", axum::routing::any(h2))
                    .route("/__admin/{*rest}", axum::routing::any(h3))
                    .fallback_service(tenant_admin)
            }
            None => {
                let h1 = make_admin_handler(tenant_admin.clone());
                let h2 = make_admin_handler(tenant_admin.clone());
                let h3 = make_admin_handler(tenant_admin.clone());
                Router::new()
                    .route("/__admin", axum::routing::any(h1))
                    .route("/__admin/", axum::routing::any(h2))
                    .route("/__admin/{*rest}", axum::routing::any(h3))
                    .fallback_service(tenant_admin)
            }
        };

        // `router_with_pools` (rather than `router`) so the operator
        // console exposes /orgs/{slug}/edit. The pool handle is needed
        // because rotating `database_url` must evict the cached
        // `TenantPool` for that org so the next request rebuilds with
        // new credentials — without eviction the operator could
        // change the URL in the DB and the cached pool would happily
        // keep authenticating with stale creds until process restart.
        let operator_admin = operator_console::router_with_pools(
            self.registry,
            self.pools.clone(),
            operator_secret,
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
        axum::serve(listener, app).await?;
        Ok(())
    }
}

fn build_resolver(apex: &str) -> ChainResolver {
    ChainResolver::new()
        .push(SubdomainResolver::new(apex.to_owned()))
        .push(HeaderResolver::default())
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
