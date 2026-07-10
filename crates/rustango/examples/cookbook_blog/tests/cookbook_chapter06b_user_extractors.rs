//! Cookbook Chapter 6b — getting the current user out of a request.
//!
//! Three ways to identify *who* is making a request:
//!
//! | Pattern | Identifies | When to reach for it |
//! |---|---|---|
//! | [`SessionUser`] extractor | The browser-cookie tenant user | Multi-tenant HTML / JSON routes that share the admin's `/login` cookie. Returns `None` for anonymous — never rejects. |
//! | [`CurrentUser`] extractor + `RouterAuthExt::require_auth` | An [`AuthenticatedUser`] resolved by a backend chain | Single-pool API routes. Middleware short-circuits with 401 when no backend matches. |
//! | [`ApiKeyBackend`] inside the chain | `Authorization: Bearer <prefix.secret>` | Headless clients (CI, scripts) that cannot present a cookie. |
//!
//! This chapter exercises all three live: it boots the cookbook
//! binary, provisions a tenant + user, then drives the
//! `GET /whoami` route (defined in `apps/blog/urls.rs`) over both
//! cookie auth and the backend chain.
//!
//! Slow (~30s) — boots a fresh binary + applies migrations. Skips
//! silently if `DATABASE_URL` is unset.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter06b_user_extractors -- --test-threads=1 --nocapture`
//!
//! [`SessionUser`]: rustango::extractors::SessionUser
//! [`CurrentUser`]: rustango::tenancy::CurrentUser
//! [`AuthenticatedUser`]: rustango::tenancy::AuthenticatedUser
//! [`ApiKeyBackend`]: rustango::tenancy::auth_backends::ApiKeyBackend

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::FromRequestParts as _;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt as _;
use rustango::sql::sqlx::{self, postgres::PgConnectOptions, postgres::PgPoolOptions};
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};
use rustango::tenancy::{CurrentUser, RouterAuthExt};
use tower::ServiceExt as _;

const BIND: &str = "127.0.0.1:8868"; // chapter-unique port
const APEX: &str = "localhost";
const SESSION_SECRET: &str = "cookbook-chapter6b-test-32bytes-please!!!!";
const DB_NAME: &str = "cookbook_ch6b_dev";
const TENANT: &str = "acme";
const USERNAME: &str = "alice";
const PASSWORD: &str = "tenantpw";

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

fn db_url() -> Option<String> {
    let base = url()?;
    let trimmed = base.rsplit_once('/').map(|(prefix, _)| prefix.to_owned())?;
    Some(format!("{trimmed}/{DB_NAME}"))
}

async fn reset_db() {
    let Some(base) = url() else { return };
    let admin_pool = sqlx::PgPool::connect(&base).await.expect("connect to admin db");
    // Kick off any lingering connections from a previous failed run
    // before DROP — otherwise Postgres rejects with 55006.
    let _ = sqlx::query(&format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = '{DB_NAME}' AND pid <> pg_backend_pid()"
    ))
    .execute(&admin_pool).await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {DB_NAME}"))
        .execute(&admin_pool).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {DB_NAME}"))
        .execute(&admin_pool).await.unwrap();
}

fn manage(verb: &str, args: &[&str], db: &str) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    Command::new(bin)
        .arg(verb)
        .args(args)
        .env("DATABASE_URL", db)
        .env("RUSTANGO_APEX_DOMAIN", APEX)
        .env("RUSTANGO_BIND", BIND)
        .env("RUSTANGO_SESSION_SECRET", SESSION_SECRET)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("manage spawn")
}

fn spawn_server(db: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_cookbook_blog");
    Command::new(bin)
        .env("DATABASE_URL", db)
        .env("RUSTANGO_APEX_DOMAIN", APEX)
        .env("RUSTANGO_BIND", BIND)
        .env("RUSTANGO_SESSION_SECRET", SESSION_SECRET)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("server spawn")
}

async fn wait_ready() {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if reqwest::Client::new()
            .get(format!("http://{BIND}/login"))
            .header("Host", format!("{TENANT}.{APEX}"))
            .send()
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("server didn't become ready within 20s");
}

/// Extract the API key token printed by `manage create-api-key`. The
/// command writes three lines; the second one is `  <prefix>.<secret>`.
fn parse_api_key(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.contains('.') && !l.contains(' '))
        .unwrap_or_else(|| panic!("create-api-key did not print a token; stdout: {stdout}"))
        .to_owned()
}

/// Per-tenant pool that points at the right schema. In schema-tenancy
/// mode each tenant lives in `<slug>` schema inside the registry DB,
/// so we set `search_path` on connect to make the `rustango_users` /
/// `rustango_api_keys` lookups inside the auth backends route to the
/// right place.
async fn tenant_pool(db: &str) -> sqlx::PgPool {
    let opts: PgConnectOptions = db.parse().expect("parse db url");
    let opts = opts.options([("search_path", &format!("{TENANT},public") as &str)]);
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .expect("tenant pool connect")
}

// ============================================================================
// §6b.1 — `SessionUser` extractor end-to-end via the cookie login flow.
// ============================================================================
//
// The route lives in `apps/blog/urls.rs`:
//
//     async fn whoami(SessionUser(user): SessionUser) -> Response {
//         match user {
//             Some(u) => Json(json!({"username": u.username})).into_response(),
//             None    => (StatusCode::UNAUTHORIZED, "anonymous").into_response(),
//         }
//     }
//
// `SessionUser` is **infallible** — it returns `None` for anonymous
// rather than rejecting. The handler decides whether to gate.
#[tokio::test]
async fn session_user_resolves_browser_cookie_and_falls_back_to_anonymous() {
    let Some(db) = db_url() else { return };
    reset_db().await;

    // Bootstrap registry + tenant + user.
    let m = manage("migrate", &[], &db);
    assert!(m.status.success(), "migrate: {}", String::from_utf8_lossy(&m.stderr));
    let m = manage("create-operator", &["admin", "--password", "letmein"], &db);
    assert!(m.status.success(), "create-operator: {}", String::from_utf8_lossy(&m.stderr));
    let m = manage(
        "create-tenant",
        &[TENANT, "--display-name", TENANT, "--host-pattern", &format!("{TENANT}.{APEX}")],
        &db,
    );
    assert!(m.status.success(), "create-tenant: {}", String::from_utf8_lossy(&m.stderr));
    let m = manage(
        "create-user",
        &[TENANT, USERNAME, "--password", PASSWORD, "--superuser"],
        &db,
    );
    assert!(m.status.success(), "create-user: {}", String::from_utf8_lossy(&m.stderr));

    // Boot.
    let mut server = spawn_server(&db);
    wait_ready().await;

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let host = format!("{TENANT}.{APEX}");

    // Step 1 — anonymous request to /whoami → 401.
    let resp = client
        .get(format!("http://{BIND}/whoami"))
        .header("Host", &host)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "anonymous /whoami should be 401");

    // Step 2 — log in via the tenant `/login` cookie route.
    let resp = client
        .post(format!("http://{BIND}/login"))
        .header("Host", &host)
        .form(&[("username", USERNAME), ("password", PASSWORD)])
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection() || resp.status().is_success(),
        "login status: {} body: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Step 3 — same client (cookie jar holds rustango_tenant_session)
    // → /whoami returns the user's identity.
    let resp = client
        .get(format!("http://{BIND}/whoami"))
        .header("Host", &host)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "logged-in /whoami should be 200");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["username"], USERNAME);
    assert_eq!(body["is_superuser"], true);

    // Step 4 — cookie minted on `acme` must NOT authenticate on a
    // different host. Even with the cookie present, the SessionUser
    // resolver treats the request as anonymous because the slug
    // binding fails.
    let resp = client
        .get(format!("http://{BIND}/whoami"))
        .header("Host", "globex.localhost")
        .send()
        .await
        .unwrap();
    // The tenant doesn't exist on `globex` → /whoami still returns 401
    // (Tenant resolution fails first; SessionUser yields None).
    assert!(
        matches!(resp.status().as_u16(), 401 | 404),
        "cross-tenant cookie must not authenticate; got {}",
        resp.status()
    );

    // ====================================================================
    // §6b.2 — `CurrentUser` + `RouterAuthExt::require_auth` against the
    // tenant pool. Demonstrates the multi-backend chain
    // (`ModelBackend` for HTTP Basic, `ApiKeyBackend` for bearer).
    // ====================================================================
    //
    // `require_auth` takes a single `PgPool`. For a multi-tenant deploy
    // that means resolving the per-tenant pool first (here we hand-build
    // one with `search_path` pointed at the tenant's schema) and wiring
    // it into the middleware at route-construction time.

    // Issue an API key for alice via `manage create-api-key`. This
    // exercises the schema-mode tenant pool fix in
    // `tenancy::manage::roles::tenant_pool_for_slug`.
    let out = manage("create-api-key", &[TENANT, USERNAME, "--label", "ci"], &db);
    assert!(
        out.status.success(),
        "create-api-key: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let api_key = parse_api_key(&out);
    assert!(api_key.contains('.'), "api key must be `prefix.secret`");

    // Build a tenant-scoped pool (schema mode → set `search_path` so
    // `rustango_users` / `rustango_api_keys` resolve to the tenant's
    // schema) for the require_auth middleware below.
    let pool = tenant_pool(&db).await;
    let backends: Vec<Arc<dyn AuthBackend>> = vec![
        Arc::new(ModelBackend),
        Arc::new(ApiKeyBackend),
    ];
    async fn profile(CurrentUser(user): CurrentUser) -> axum::response::Response {
        match user {
            Some(u) => axum::Json(serde_json::json!({
                "id": u.id,
                "username": u.username,
                "is_superuser": u.is_superuser,
            }))
            .into_response(),
            // Unreachable when require_auth is in the stack, but kept
            // here so the handler is independently sane.
            None => (StatusCode::UNAUTHORIZED, "anonymous").into_response(),
        }
    }
    let app: Router = Router::new()
        .route("/profile", get(profile))
        .require_auth(backends, pool.clone().into());

    // (a) No credentials → 401.
    let (status, _body) = oneshot(app.clone(), Method::GET, "/profile", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "missing auth must 401");

    // (b) HTTP Basic with right password → 200 with username.
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD
        .encode(format!("{USERNAME}:{PASSWORD}"));
    let (status, body) = oneshot(
        app.clone(),
        Method::GET,
        "/profile",
        None,
        Some(("Authorization", &format!("Basic {basic}"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "Basic auth should succeed; body: {body}");
    assert_eq!(body["username"], USERNAME);
    assert_eq!(body["is_superuser"], true);

    // (c) HTTP Basic with WRONG password → 401 (no backend accepted).
    let bad = base64::engine::general_purpose::STANDARD
        .encode(format!("{USERNAME}:wrong-password"));
    let (status, _body) = oneshot(
        app.clone(),
        Method::GET,
        "/profile",
        None,
        Some(("Authorization", &format!("Basic {bad}"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "bad password must 401");

    // (d) Bearer with the API-key token → 200.
    let (status, body) = oneshot(
        app.clone(),
        Method::GET,
        "/profile",
        None,
        Some(("Authorization", &format!("Bearer {api_key}"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "API key should auth; body: {body}");
    assert_eq!(body["username"], USERNAME);

    // (e) Bearer with the right prefix but a wrong secret → 401.
    // ApiKeyBackend looks up the row by prefix, then verifies the
    // secret hash; mismatch returns Ok(None), required → 401.
    let tampered = format!("{}.{}", &api_key[..8], "0".repeat(api_key.len() - 9));
    let (status, _body) = oneshot(
        app,
        Method::GET,
        "/profile",
        None,
        Some(("Authorization", &format!("Bearer {tampered}"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "tampered bearer must 401");

    let _ = server.kill();
    let _ = server.wait();
}

// ----------------------------------------------------------------------------
// §6b.3 — `CurrentUser` is `Infallible` and resolves to `None` when the
// `require_auth` middleware isn't in the stack. Guards must be explicit.
// ----------------------------------------------------------------------------
//
// Pure compile + extract test. No DB, no network — just shows that
// dropping the middleware silently turns every user `None` rather
// than producing 401s. Forgetting `.require_auth(...)` is a foot-gun
// the typed extractor cannot catch for you.
#[tokio::test]
async fn current_user_without_middleware_is_none() {
    let req = Request::builder().uri("/").body(()).unwrap();
    let (mut parts, _) = req.into_parts();
    let CurrentUser(user) =
        CurrentUser::from_request_parts(&mut parts, &()).await.unwrap();
    assert!(user.is_none(), "no middleware in stack → CurrentUser is None");
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

async fn oneshot(
    router: Router,
    method: Method,
    uri: &str,
    body: Option<&str>,
    auth_header: Option<(&str, &str)>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = auth_header {
        req = req.header(k, v);
    }
    let body = match body {
        Some(s) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(s.to_owned())
        }
        None => Body::empty(),
    };
    let resp = router.oneshot(req.body(body).unwrap()).await.expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}
