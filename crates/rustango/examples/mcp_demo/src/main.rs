//! Runnable Model Context Protocol (MCP) server — the live vehicle behind
//! `docs/mcp.md`.
//!
//! A single SQLite-backed tenant (`acme`) exposes one tool (`add`) to an
//! authorized agent over the MCP Streamable-HTTP transport. No external
//! services: the tenant's data lives in a temp SQLite file and the
//! agent + skill + grant are seeded at startup, so every `cargo run`
//! prints a fresh agent credential you can paste straight into the
//! MCP Inspector.
//!
//! Run:
//! ```bash
//! cargo run            # → listens on http://localhost:8090/mcp
//! ```
//! then mint a token and call the tool:
//! ```bash
//! TOKEN=$(curl -s http://localhost:8090/mcp/token \
//!     -H 'X-Org: acme' -H 'content-type: application/json' \
//!     -d '{"name":"demo-bot","secret":"<printed-secret>"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["access_token"])')
//!
//! curl -s http://localhost:8090/mcp -H "X-Org: acme" \
//!     -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
//!     -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
//!          "params":{"name":"add","arguments":{"a":2,"b":3}}}'
//! # → {"result":{"structuredContent":{"sum":5}, ...}}
//! ```

use std::sync::Arc;

use axum::extract::Request;
use axum::{middleware, Router};
use rustango::extractors::TenantContext;
use rustango::mcp::{McpContext, McpError};
use rustango::sql::sqlx::{self, SqlitePool};
use rustango::sql::{Auto, Pool};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use rustango::tenancy::session::SessionSecret;
use rustango::tenancy::{
    create_agent_pool, create_skill_pool, grant_skill_pool, ChainResolver, Org, OrgResolver,
    TenancyError, TenantPools,
};
use serde_json::json;

const TENANT_SLUG: &str = "acme";
const AGENT_NAME: &str = "demo-bot";
const SKILL_SLUG: &str = "calculator";
const BIND: &str = "127.0.0.1:8090";

// ── Step 2: the tool the agent may call ─────────────────────────────────────
rustango::register_mcp_tool!(
    "add",
    "Add two integers and return their sum",
    AddInput,
    |_ctx: McpContext, input: AddInput| async move {
        Ok::<_, McpError>(json!({ "sum": input.a + input.b }))
    },
);

#[derive(serde::Deserialize)]
struct AddInput {
    a: i64,
    b: i64,
}

impl rustango::openapi::OpenApiSchema for AddInput {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::object()
            .property("a", rustango::openapi::Schema::integer())
            .property("b", rustango::openapi::Schema::integer())
            .required(["a", "b"])
    }
}

// ── tenant resolution ───────────────────────────────────────────────────────
// Synthetic resolver: maps `X-Org: acme` (or `Host: acme.*`) to the one
// SQLite-backed tenant, skipping a registry database entirely. Real
// deployments provision orgs in a registry and let the built-in
// `HeaderResolver` / `SubdomainResolver` look them up — see the
// `create_tenant_if_missing` recipe in the guide.
#[derive(Clone)]
struct AcmeResolver {
    org: Org,
}

#[async_trait::async_trait]
impl OrgResolver for AcmeResolver {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &Pool,
    ) -> Result<Option<Org>, TenancyError> {
        // Honor an explicit `X-Org` header (and isolate other tenants);
        // when none is sent, fall back to the single demo tenant so a
        // generic MCP client — e.g. the Inspector, which sends only the
        // `localhost` Host — still resolves `acme`.
        match parts.headers.get("x-org").and_then(|v| v.to_str().ok()) {
            Some(slug) if slug == TENANT_SLUG => Ok(Some(self.org.clone())),
            Some(_) => Ok(None), // a different tenant — not provisioned here
            None => Ok(Some(self.org.clone())),
        }
    }
}

fn acme_org(database_url: String) -> Org {
    Org {
        id: Auto::default(),
        slug: TENANT_SLUG.into(),
        display_name: "Acme Inc.".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(database_url),
        schema_name: None,
        host_pattern: Some("acme.localhost".into()),
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
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Fresh tenant DB each run → a freshly minted, printable agent credential.
    let db_path = std::env::temp_dir().join("rustango_mcp_demo_acme.db");
    let _ = std::fs::remove_file(&db_path);
    let tenant_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let org = acme_org(tenant_url);

    // Registry pool is unused (the resolver is synthetic) but `TenantPools`
    // requires one; a lazy in-memory handle is never connected to.
    let registry: SqlitePool = SqlitePool::connect_lazy("sqlite::memory:")?;
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(registry));

    // ── Step 4: seed an agent + a skill that grants `add` ───────────────────
    let tenant_pool: Pool = pools.scoped_pool_dyn(&org).await?;
    let issued = create_agent_pool(&tenant_pool, AGENT_NAME).await?;
    create_skill_pool(
        &tenant_pool,
        SKILL_SLUG,
        "Calculator",
        "Basic arithmetic over MCP",
        "You are a precise calculator. Use the `add` tool for sums.",
        &["add".into()],
    )
    .await?;
    grant_skill_pool(&tenant_pool, TENANT_SLUG, AGENT_NAME, SKILL_SLUG).await?;

    // ── Step 3: mount the agent-guarded MCP router ──────────────────────────
    // Keys come from the environment, else random for this process: tokens
    // stay valid while it runs, and no key lives in the source.
    let jwt = Arc::new(JwtLifecycle::new(jwt_secret()));
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: pools.clone(),
        resolver: ChainResolver::new().push(AcmeResolver { org }),
        session_secret: SessionSecret::from_env_or_random(),
        // The demo never signs operator sessions; a throwaway key.
        operator_secret: SessionSecret::from_bytes(
            rustango::random::get_random_token_urlsafe(48).into_bytes(),
        ),
    });

    let app: Router = Router::new()
        .nest("/mcp", rustango::mcp::tenant_router_authed(jwt))
        .layer(middleware::from_fn(
            move |mut req: Request, next: middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));

    println!();
    println!("  rustango MCP demo  →  http://{BIND}/mcp");
    println!("  ──────────────────────────────────────────────");
    println!("  tenant header : X-Org: {TENANT_SLUG}");
    println!("  agent name    : {AGENT_NAME}");
    println!("  agent secret  : {}", issued.token);
    println!();
    println!("  Mint a bearer token:");
    println!("    curl -s http://localhost:8090/mcp/token \\");
    println!("      -H 'X-Org: {TENANT_SLUG}' -H 'content-type: application/json' \\");
    println!(
        "      -d '{{\"name\":\"{AGENT_NAME}\",\"secret\":\"{}\"}}'",
        issued.token
    );
    println!();

    let listener = tokio::net::TcpListener::bind(BIND).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `MCP_DEMO_JWT_SECRET` (32+ bytes), else a random key for this process.
fn jwt_secret() -> Vec<u8> {
    match std::env::var("MCP_DEMO_JWT_SECRET") {
        Ok(s) if s.len() >= 32 => s.into_bytes(),
        Ok(_) => panic!("MCP_DEMO_JWT_SECRET must be at least 32 bytes"),
        Err(_) => rustango::random::get_random_token_urlsafe(64).into_bytes(),
    }
}
