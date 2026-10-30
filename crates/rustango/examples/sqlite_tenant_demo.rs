//! Multi-tenant SQLite demo — minimal end-to-end wiring of a
//! database-per-tenant stack (`DatabaseTenantContext<Sqlite>`), queried
//! through the ORM on the `TenantScope` extractor's tenant pool.
//!
//! Runs without external state: the registry is a lazy in-memory
//! SQLite pool (the resolver in this demo is hardcoded, so it is
//! never actually connected to), and each tenant's data lives in
//! in-memory SQLite.
//!
//! Run:
//!
//! ```bash
//! cargo run --example sqlite_tenant_demo \
//!     --features=admin,postgres,runserver,tenancy,sqlite,template_views
//! curl -H "Host: acme.localhost" http://localhost:8090/whoami
//! # → acme: 1 post(s)
//! curl -H "Host: beta.localhost" http://localhost:8090/whoami
//! # → beta: 1 post(s)
//! ```
//!
//! Each request adds a post, so the count grows per tenant. It listens
//! on `127.0.0.1:8090`; set `RUSTANGO_BIND` to change that.
//!
//! Each tenant has its own SQLite database; the `posts` table is
//! created on the first request from the model's schema.

use std::sync::Arc;

use axum::extract::Request;
use axum::middleware;
use axum::routing::get;
use axum::Router;
use rustango::extractors::{DatabaseTenantContext, TenantScope};
use rustango::sql::{sqlx, Auto, CounterPool as _};
use rustango::tenancy::{
    operator_console::SessionSecret, BackendKind, ChainResolver, DatabasePools, Org, OrgResolver,
    TenancyError,
};

/// Resolver that maps `Host: <slug>.localhost` to a synthetic Org
/// row. Skips the registry entirely — the demo doesn't need a real
/// Postgres just to demonstrate the per-tenant SQLite path.
#[derive(Clone)]
struct HostResolver;

#[async_trait::async_trait]
impl OrgResolver for HostResolver {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, TenancyError> {
        let host = parts
            .headers
            .get(axum::http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let slug = host.split('.').next().unwrap_or("");
        if slug.is_empty() {
            return Ok(None);
        }
        Ok(Some(Org {
            id: rustango::sql::Auto::default(),
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            // The pool registry handles `database_url=None` by
            // expanding the configured `{slug}` template. The demo
            // template lives in memory keyed per slug.
            database_url: None,
            schema_name: None,
            host_pattern: None,
            port: None,
            path_prefix: None,
            active: true,
            created_at: chrono::Utc::now(),
            brand_name: None,
            brand_tagline: None,
            logo_path: None,
            favicon_path: None,
            primary_color: None,
            theme_mode: None,
        }))
    }
}

/// A tenant's post; each tenant's database has its own `posts` table.
#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "posts")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

/// Per-request handler: adds a post to the tenant's database and
/// reports the count. `t.pool()` reaches only that tenant's database.
async fn whoami(t: TenantScope) -> Result<String, (axum::http::StatusCode, String)> {
    use rustango::core::Model as _;
    let pool = t.pool();
    // Real apps run migrations; the demo creates its one table inline.
    let ddl = rustango::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        pool.dialect(),
        Post::SCHEMA,
    );
    rustango::sql::raw_execute_pool(pool, &ddl, Vec::new())
        .await
        .map_err(rustango::server_error)?;
    let mut post = Post {
        id: Auto::default(),
        title: "hello".into(),
    };
    post.insert_pool(pool)
        .await
        .map_err(rustango::server_error)?;
    let n = Post::objects()
        .count(pool)
        .await
        .map_err(rustango::server_error)?;
    Ok(format!("{}: {n} post(s)\n", t.org.slug))
}

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite)
            // `{slug}` expansion produces a unique in-memory DB per
            // tenant (`file:` + `cache=shared` keeps the connection
            // pool's checkouts pointed at the SAME in-memory DB so
            // writes persist across pool acquires).
            .with_url_template("sqlite:file:demo_{slug}?mode=memory&cache=shared"),
    );

    let resolver = ChainResolver::new().push(HostResolver);

    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver,
        // `RUSTANGO_SESSION_SECRET`, else random for this process. The demo
        // never signs operator sessions, so that key is throwaway.
        session_secret: SessionSecret::from_env_or_random(),
        operator_secret: SessionSecret::from_bytes(
            rustango::random::get_random_token_urlsafe(48).into_bytes(),
        ),
        // v0.34 B.2 — the demo's registry is itself SQLite, proving
        // a pure-SQLite stack works end-to-end (no Postgres anywhere).
        registry: rustango::sql::Pool::Sqlite(sqlx::SqlitePool::connect_lazy("sqlite::memory:")?),
    });

    let app: Router = Router::new()
        .route("/whoami", get(whoami))
        .layer(middleware::from_fn(
            move |mut req: Request, next: middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));

    let bind = std::env::var("RUSTANGO_BIND").unwrap_or_else(|_| "127.0.0.1:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("sqlite_tenant_demo listening on {bind}");
    println!("try: curl -H 'Host: acme.localhost' http://{bind}/whoami");
    axum::serve(listener, app).await?;
    Ok(())
}
