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
use rustango::Model;
use rustango_tenancy::{
    admin::TenantAdminBuilder,
    manage,
    operator_console::{self, SessionSecret},
    ChainResolver, HeaderResolver, Org, SubdomainResolver, TenantPools,
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

    // ---- 1. Reset every relevant table for a clean demo run ----
    //
    // `drop_all` walks every linked `#[derive(Model)]`, including the
    // tenancy registry tables and our `Post` model. CASCADE means we
    // don't have to topologically sort. This wipes the demo's state in
    // the registry; tenant schemas get dropped explicitly below.
    println!("==> dropping every linked table for a clean slate");
    rustango::migrate::drop_all(&registry).await?;
    // Also drop the migration ledger so the second run sees a fresh
    // ledger and re-applies the bootstraps. `__rustango_migrations__`
    // is owned by the runner, not by inventory, so drop_all leaves it
    // alone.
    sqlx::query(r#"DROP TABLE IF EXISTS "__rustango_migrations__" CASCADE"#)
        .execute(&registry)
        .await?;

    // ---- 2. Bootstrap registry tables via the packaged migrations ----
    let migrations_dir = std::env::temp_dir().join("rustango_multitenant_demo_migrations");
    let _ = std::fs::remove_dir_all(&migrations_dir);
    {
        let mut buf: Vec<u8> = Vec::new();
        manage::run_with_writer(
            &pools,
            &registry_url,
            &migrations_dir,
            vec!["init-tenancy".into()],
            &mut buf,
        )
        .await?;
        manage::run_with_writer(
            &pools,
            &registry_url,
            &migrations_dir,
            vec!["migrate-registry".into()],
            &mut buf,
        )
        .await?;
        print!("{}", String::from_utf8_lossy(&buf));
    }

    // ---- 3. Provision two schema-mode tenants ----
    for slug in [ACME, GLOBEX] {
        drop_schema(&registry, slug).await?;

        // create-tenant (no `--no-migrate`) creates the schema, inserts
        // the Org row, and runs the packaged tenant-scoped bootstrap
        // migration against the new schema — so `rustango_users` lands
        // automatically.
        let mut buf: Vec<u8> = Vec::new();
        manage::run_with_writer(
            &pools,
            &registry_url,
            &migrations_dir,
            vec![
                "create-tenant".into(),
                slug.into(),
                "--mode".into(),
                "schema".into(),
                "--display-name".into(),
                slug.to_uppercase(),
            ],
            &mut buf,
        )
        .await?;
        // The demo's own `Post` model has no migration JSON — hand-roll
        // it inside the tenant schema. Real apps would author a tenant-
        // scoped migration and let `migrate-tenants` apply it.
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
        manage::run_with_writer(
            &pools,
            &registry_url,
            &migrations_dir,
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

    // Bootstrap a default operator so login works out of the box.
    // Real deployments use `manage create-operator <user> --password
    // <p>` — for the demo we make sure `admin / letmein` exists.
    {
        let mut buf: Vec<u8> = Vec::new();
        // create-operator is idempotent only by check-then-insert;
        // ignore "already exists" so re-runs don't break.
        let res = manage::run_with_writer(
            &pools,
            &registry_url,
            std::path::Path::new(""),
            vec![
                "create-operator".into(),
                "admin".into(),
                "--password".into(),
                "letmein".into(),
            ],
            &mut buf,
        )
        .await;
        match res {
            Ok(()) => print!("{}", String::from_utf8_lossy(&buf)),
            Err(e) if e.to_string().contains("already exists") => {}
            Err(e) => return Err(e.into()),
        }
    }

    // Operator console: form-based login + sidebar layout. Replaces
    // the previous HTTP-Basic-on-rustango-admin combination. The
    // session secret comes from `RUSTANGO_SESSION_SECRET` (base64
    // 32+ bytes) or a random fallback (warned at boot).
    let session_secret = SessionSecret::from_env_or_random();
    let operator_admin = operator_console::router(registry.clone(), session_secret);

    // Host-based dispatch — matches the production routing story
    // exactly: bare apex (`localhost` here, `app.example.com` in
    // prod) hosts the operator UI; subdomains (`acme.localhost`,
    // `globex.localhost`) host tenant UIs.
    //
    // Why not `nest("/operator", ...)`? The bundled
    // `rustango-admin` templates emit absolute paths like
    // `<a href="/rustango_orgs">`, which don't include any nest
    // prefix. Mounting the operator under `/operator` means
    // every admin link breaks (404s the tenant fallback). Host
    // dispatch sidesteps the issue and is what production should
    // do anyway.
    let app = axum::Router::new().fallback_service(tower::service_fn({
        let operator = operator_admin.clone();
        let tenants = tenant_admin.clone();
        let apex = "localhost".to_owned();
        move |req: axum::http::Request<axum::body::Body>| {
            let mut operator = operator.clone();
            let mut tenants = tenants.clone();
            let apex = apex.clone();
            async move {
                use tower::ServiceExt as _;
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

    println!();
    println!("==> serving on 0.0.0.0:8080");
    println!("    http://acme.localhost:8080/post     ACME's posts");
    println!("    http://globex.localhost:8080/post   Globex's posts");
    println!("    http://other.localhost:8080/post    404 (no tenant)");
    println!("    http://localhost:8080/              operator console");
    println!("        sign in with admin / letmein at /login");
    println!("        — operator lives at the apex (no subdomain).");
    println!("          subdomains route to tenant UIs via the resolver.");
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
