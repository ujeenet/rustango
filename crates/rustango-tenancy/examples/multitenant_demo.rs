//! `multitenant_demo` — end-to-end v0.5 multi-tenancy showcase.
//!
//! Boots a registry, provisions two schema-mode tenants (acme + globex),
//! inserts per-tenant data, and serves a tenant-aware admin under
//! `*.localhost:8080`. Browsers resolve `acme.localhost` and
//! `globex.localhost` to 127.0.0.1 automatically — no `/etc/hosts` edits
//! and no DNS infra needed for the demo.
//!
//! ## Run
//!
//! ```sh
//! docker compose up -d                               # local Postgres
//! DATABASE_URL=postgres://rustango:rustango@localhost:5432/rustango_test \
//! RUSTANGO_APEX_DOMAIN=localhost \
//! PATH="$HOME/.cargo/bin:$PATH" \
//!     cargo run --example multitenant_demo -p rustango-tenancy
//! ```
//!
//! Then open in a browser:
//!
//! * <http://acme.localhost:8080/post>     — ACME's posts (only ACME's)
//! * <http://globex.localhost:8080/post>   — Globex's posts (only Globex's)
//! * <http://localhost:8080/operator/>     — operator UI: list of all
//!   registry models including `rustango_orgs` so you can see both tenants
//! * <http://other.localhost:8080/post>    — 404 (no tenant matches)
//!
//! Both tenants have a superuser called `alice` with password `hunter2`
//! — HTTP Basic auth is wired into the inner admin via the existing
//! single-credential helper for the demo (slice 6's database-backed auth
//! is library-level; admin-side wiring lands in v0.6.x).

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::sqlx::{self, PgPool};
use rustango::sql::Fetcher;
use rustango::{migrate as rmig, Model};
use rustango_tenancy::{
    admin::TenantAdminBuilder, manage, ChainResolver, HeaderResolver, Org, SubdomainResolver,
    TenantPools,
};

/// Per-tenant Post model. Lives in the tenant's schema in this demo.
#[derive(Model, Debug, Clone)]
#[rustango(table = "post", display = "title")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 8000)]
    pub body: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const ACME: &str = "acme";
const GLOBEX: &str = "globex";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let registry_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgres://rustango:rustango@localhost:5432/rustango_test".into()
        });
    let registry = PgPool::connect(&registry_url).await?;
    let pools = Arc::new(TenantPools::new(registry.clone()));

    // ---- 1. Reset the registry and bootstrap every registered model ----
    println!("==> dropping and recreating registry tables");
    rmig::drop_all(&registry).await?;
    rmig::apply_all(&registry).await?;

    // ---- 2. Provision two schema-mode tenants ----
    for slug in [ACME, GLOBEX] {
        drop_schema(&registry, slug).await?;
        // Manually CREATE SCHEMA + the post + user tables in each
        // tenant's schema. In a real deployment you'd write a
        // tenant-scoped migration JSON and run `manage migrate-tenants`;
        // for the demo we keep it inline so the example is self-contained.
        run(&registry, &format!(r#"CREATE SCHEMA "{slug}""#)).await?;
        run(
            &registry,
            &format!(
                r#"CREATE TABLE "{slug}"."post" (
                    "id" BIGSERIAL NOT NULL PRIMARY KEY,
                    "title" VARCHAR(200) NOT NULL,
                    "body" VARCHAR(8000) NOT NULL,
                    "created_at" TIMESTAMPTZ NOT NULL
                )"#
            ),
        )
        .await?;
        run(
            &registry,
            &format!(
                r#"CREATE TABLE "{slug}"."rustango_users" (
                    "id" BIGSERIAL NOT NULL PRIMARY KEY,
                    "username" VARCHAR(64) NOT NULL,
                    "password_hash" VARCHAR(255) NOT NULL,
                    "is_superuser" BOOLEAN NOT NULL,
                    "active" BOOLEAN NOT NULL,
                    "created_at" TIMESTAMPTZ NOT NULL
                )"#
            ),
        )
        .await?;

        // Use the manage runner to provision the Org row + a superuser.
        // This is the same code path operators use in production.
        let mut buf: Vec<u8> = Vec::new();
        manage::run_with_writer(
            &pools,
            &registry_url,
            std::path::Path::new(""),
            vec![
                "create-tenant".into(),
                slug.into(),
                "--mode".into(),
                "schema".into(),
                "--display-name".into(),
                slug.to_uppercase(),
                "--no-migrate".into(),
            ],
            &mut buf,
        )
        .await?;
        manage::run_with_writer(
            &pools,
            &registry_url,
            std::path::Path::new(""),
            vec![
                "create-user".into(),
                slug.into(),
                "alice".into(),
                "--password".into(),
                "hunter2".into(),
                "--superuser".into(),
            ],
            &mut buf,
        )
        .await?;
        print!("{}", String::from_utf8_lossy(&buf));
    }

    // ---- 3. Insert a Post per tenant via TenantPools ----
    for slug in [ACME, GLOBEX] {
        let org = lookup_org(&registry, slug).await?;
        // For schema mode, run the INSERT through a connection acquired
        // via TenantPools::acquire so search_path is set first.
        let mut conn = pools.acquire(&org).await?;
        let conn_ref: &mut sqlx::PgConnection = &mut conn;
        sqlx::query(
            "INSERT INTO post (title, body, created_at) VALUES ($1, $2, $3::timestamptz)",
        )
        .bind(format!("Hello from {slug}"))
        .bind(format!("This post lives only in {slug}'s schema."))
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(conn_ref)
        .await?;
    }

    // ---- 4. Build the tenant-aware admin ----
    // Subdomain-first: `acme.localhost:8080` resolves to acme via
    // `SubdomainResolver`. The HeaderResolver fallback lets curl hit
    // the demo without futzing with browser DNS — `curl -H "X-Org: acme"
    // http://localhost:8080/post` works too.
    let resolver = ChainResolver::new()
        .push(SubdomainResolver::new("localhost"))
        .push(HeaderResolver::default());

    let tenant_admin = TenantAdminBuilder::new(pools.clone(), registry_url.clone(), resolver)
        .show_only(["post"])
        .build();

    let operator_admin = rustango::admin::router(registry.clone());
    let operator_admin = rustango::admin::protect_with_basic_auth(
        operator_admin,
        "operator",
        "letmein",
    );

    // Operator routes mount at `/operator/*` and take precedence;
    // everything else falls through to the tenant-aware admin (which
    // does its own resolver dispatch). `fallback_service` is the
    // axum idiom for "if no specific route matched, hand off to this
    // sub-Router" — needed because TenantAdminBuilder uses a global
    // fallback internally.
    let app = axum::Router::new()
        .nest("/operator", operator_admin)
        .fallback_service(tenant_admin);

    println!();
    println!("==> serving on 0.0.0.0:8080");
    println!("    http://acme.localhost:8080/post     ACME's posts");
    println!("    http://globex.localhost:8080/post   Globex's posts");
    println!("    http://other.localhost:8080/post    404 (no tenant)");
    println!("    http://localhost:8080/operator      operator UI (basic auth: operator/letmein)");
    println!("        — note: NO trailing slash. axum's `nest()` doesn't");
    println!("          rewrite `/operator/` to the inner admin's `/`.");
    println!("    curl  http://localhost:8080/post -H 'X-Org: acme'");
    println!();

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn run(pool: &PgPool, sql: &str) -> Result<(), sqlx::Error> {
    sqlx::query(sql).execute(pool).await?;
    Ok(())
}

async fn drop_schema(pool: &PgPool, name: &str) -> Result<(), sqlx::Error> {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await?;
    Ok(())
}

async fn lookup_org(pool: &PgPool, slug: &str) -> Result<Org, Box<dyn std::error::Error>> {
    let mut rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await?;
    rows.pop().ok_or_else(|| format!("org `{slug}` not found").into())
}
