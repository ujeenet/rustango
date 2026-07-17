//! Multi-tenant SQLite demo — minimal end-to-end wiring of v0.33's
//! `DatabaseTenant<Sqlite>` extractor.
//!
//! Runs without external state: spins up a Postgres-shaped registry
//! against a lazy pool (the resolver in this demo is hardcoded, so
//! the registry pool is never actually connected to), and uses
//! in-memory SQLite for each tenant's data.
//!
//! Run:
//!
//! ```bash
//! cargo run --example sqlite_tenant_demo \
//!     --features=admin,postgres,runserver,tenancy,sqlite,template_views
//! curl -H "Host: acme.localhost" http://localhost:8090/whoami
//! # → acme: 1 post
//! curl -H "Host: beta.localhost" http://localhost:8090/whoami
//! # → beta: 1 post
//! ```
//!
//! Each tenant has its own SQLite database; the `posts` table is
//! created on first acquire via a small bootstrap.

use std::sync::Arc;

use axum::extract::Request;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::{DatabaseTenant, DatabaseTenantContext};
use rustango::sql::sqlx::{self, Row};
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
            sso_enabled: false,
            sso_provider: None,
            sso_issuer_url: None,
            sso_client_id: None,
            sso_secret_ref: None,
        }))
    }
}

/// Per-request handler — counts posts in the tenant's DB and
/// reports the count. Bootstraps an empty schema on first hit so
/// every tenant lands on the same starting point.
async fn whoami(mut t: DatabaseTenant<sqlx::Sqlite>) -> impl IntoResponse {
    // Idempotent bootstrap. Real apps would run this via migrations;
    // the demo keeps it inline to stay single-file.
    sqlx::query("CREATE TABLE IF NOT EXISTS posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)")
        .execute(&mut ***t.conn())
        .await
        .expect("create");
    sqlx::query("INSERT INTO posts (title) VALUES ('hello')")
        .execute(&mut ***t.conn())
        .await
        .expect("insert");
    let row = sqlx::query("SELECT COUNT(*) as n FROM posts")
        .fetch_one(&mut ***t.conn())
        .await
        .expect("count");
    let n: i64 = row.try_get("n").expect("n");
    format!("{}: {n} posts\n", t.org.slug)
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
        session_secret: SessionSecret::from_bytes(b"demo_tenant_secret____32bytes!!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"demo_operator_secret___32bytes!!".to_vec()),
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

    let bind = std::env::var("RUSTANGO_BIND").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("sqlite_tenant_demo listening on {bind}");
    println!("try: curl -H 'Host: acme.localhost' http://localhost:8090/whoami");
    axum::serve(listener, app).await?;
    Ok(())
}
