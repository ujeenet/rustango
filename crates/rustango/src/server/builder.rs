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
    api: Option<ApiRouter>,
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
            api: None,
        })
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

    /// Apply every migration in `dir` to the registry + every active
    /// tenant. The Django-shape one-call setup:
    ///
    /// 1. Write the packaged tenancy bootstrap migrations
    ///    (`0001_rustango_registry_initial`, `0001_rustango_tenant_initial`)
    ///    into `dir` if they're not already present — idempotent.
    /// 2. Apply registry-scoped migrations against the registry pool.
    /// 3. Apply tenant-scoped migrations against every active org's
    ///    storage (schema-mode or database-mode), per-tenant
    ///    isolation: failures on one tenant don't abort the others.
    ///
    /// `dir` is created via `fs::create_dir_all` if it doesn't exist
    /// — first-run friendly. Pass the path the demo or app commits its
    /// hand-authored / `make-migrations`-generated JSON files to.
    ///
    /// # Errors
    /// I/O failures creating the directory or writing the bootstrap
    /// files; [`crate::tenancy::TenancyError`] from the registry or
    /// tenant migration runners.
    pub async fn migrate<P: AsRef<std::path::Path>>(
        self,
        dir: P,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        crate::tenancy::init_tenancy(dir)?;
        let _ = crate::tenancy::migrate_registry(&self.pools, dir).await?;
        let _ = crate::tenancy::migrate_tenants(&self.pools, dir, &self.registry_url).await?;
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
        let ctx = Arc::new(TenantContext {
            pools: self.pools.clone(),
            resolver: build_resolver(&self.apex),
        });

        let session_secret_for_tenant = SessionSecret::from_env_or_random();
        let mut tenant_admin_builder = TenantAdminBuilder::new(
            self.pools.clone(),
            self.registry_url.clone(),
            resolver_for_admin,
        );
        if !self.show_only.is_empty() {
            tenant_admin_builder = tenant_admin_builder.show_only(self.show_only.clone());
        }
        let tenant_admin = tenant_admin_builder
            .with_session(session_secret_for_tenant)
            .build();

        // The user's API router (stateless) fronts the tenant admin
        // for a path-falls-through chain: `/api/...` → user; anything
        // else → admin login + CRUD pages. Every request gets the
        // `TenantContext` extension so handler-side
        // `extractors::Tenant` can resolve.
        let tenant_app = match self.api {
            Some(router) => router
                .layer(Extension(ctx.clone()))
                .fallback_service(tenant_admin),
            None => Router::new().fallback_service(tenant_admin),
        };

        let session_secret = SessionSecret::from_env_or_random();
        let operator_admin = operator_console::router(self.registry, session_secret);

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
