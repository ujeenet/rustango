//! Backing test for `docs/auth-backends.md` (and the API-key middleware half of
//! `docs/auth-api-keys.md`). Assembles a tenancy auth-backend chain
//! (`ModelBackend` for HTTP Basic + `ApiKeyBackend` for Bearer), gates routes
//! with `require_auth` / `require_perm`, and reads the user via `CurrentUser`.
//! In-memory SQLite, no external services.
//!
//! Run: `cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`

#![cfg(all(feature = "sqlite", feature = "tenancy"))]
#![allow(irrefutable_let_patterns)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::auth_backends::{
    create_api_key, ensure_api_keys_table_pool, ApiKeyBackend, AuthBackend, ModelBackend,
};
use rustango::tenancy::permissions::set_user_perm_pool;
use rustango::tenancy::{CurrentUser, RouterAuthExt};
use tower::ServiceExt;

/// Minimal base64 (standard alphabet) so we can build a Basic-auth header
/// without pulling an extra dependency into the test.
fn b64(input: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn basic(user: &str, pass: &str) -> String {
    format!("Basic {}", b64(&format!("{user}:{pass}")))
}

/// In-memory SQLite with the user table + permission tables, two users, and a
/// `post.add` grant for alice.
async fn setup() -> (Pool, i64) {
    // The same `cache=shared` database the tenant context resolves to,
    // so the fixtures and the middleware see one set of rows.
    let pool = Pool::connect(SOLE_TENANT_URL).await.expect("sqlite pool");
    // Each test re-seeds; drop what a previous one left behind.
    for t in ["rustango_users", "rustango_api_keys"] {
        let _ =
            rustango::sql::raw_execute_pool(&pool, &format!("DROP TABLE IF EXISTS {t}"), vec![])
                .await;
    }
    // rustango_users from `User::SCHEMA` (no hand-written DDL to drift).
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("create users");
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("ensure perm tables");
    ensure_api_keys_table_pool(&pool)
        .await
        .expect("ensure api keys table");

    seed_user(&pool, "alice", "s3cret").await;
    seed_user(&pool, "bob", "bobpw").await;
    let alice_id = user_id(&pool, "alice").await;
    set_user_perm_pool(alice_id, "post.add", true, &pool)
        .await
        .expect("grant perm");
    (pool, alice_id)
}

async fn seed_user(pool: &Pool, username: &str, password: &str) {
    let hash = rustango::tenancy::password::hash(password).expect("hash");
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES (?, ?, 0, 1, datetime('now'))",
    )
    .bind(username)
    .bind(&hash)
    .execute(sq)
    .await
    .expect("seed user");
}

async fn user_id(pool: &Pool, username: &str) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM rustango_users WHERE username = ?")
        .bind(username)
        .fetch_one(sq)
        .await
        .expect("lookup id");
    id
}

/// A handler that reads the authenticated user (or 401 if anonymous — though
/// `require_auth` already short-circuits that case before we get here).
async fn profile(CurrentUser(user): CurrentUser) -> axum::response::Response {
    use axum::response::IntoResponse;
    match user {
        Some(u) => format!("hello {}", u.username).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn admin_only() -> &'static str {
    "secret area"
}

/// Resolver that hands every request the same single-tenant Org.
///
/// `require_auth` takes no pool as of 0.57.11 — it reads
/// the tenant resolved for each request, so even a one-tenant app has
/// to mount a context for it to read. That is the cost of the fix and
/// it is deliberate: a pool chosen once per process is exactly what
/// let a credential from one tenant authenticate on another's host.
#[derive(Clone)]
struct SoleTenant;

#[async_trait::async_trait]
impl rustango::tenancy::OrgResolver for SoleTenant {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &Pool,
    ) -> Result<Option<rustango::tenancy::Org>, rustango::tenancy::TenancyError> {
        Ok(Some(rustango::tenancy::Org {
            id: rustango::sql::Auto::default(),
            slug: "sole".into(),
            display_name: "sole".into(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: Some(SOLE_TENANT_URL.to_owned()),
            ..rustango::testkit::org()
        }))
    }
}

/// One shared in-memory database for the whole test — `cache=shared`
/// so the pool the context builds and the pool the fixtures seed are
/// the same database.
const SOLE_TENANT_URL: &str = "sqlite:file:auth_backends_doc?mode=memory&cache=shared";

fn app() -> Router {
    // The chain: HTTP Basic (ModelBackend) first, then Bearer API key.
    let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend), Arc::new(ApiKeyBackend)];

    let ctx = Arc::new(rustango::extractors::DatabaseTenantContext {
        pools: Arc::new(
            rustango::tenancy::DatabasePools::<sqlx::Sqlite>::new(
                rustango::tenancy::BackendKind::Sqlite,
            )
            .with_url_template(SOLE_TENANT_URL),
        ),
        resolver: rustango::tenancy::ChainResolver::new().push(SoleTenant),
        session_secret: rustango::tenancy::session::SessionSecret::from_bytes(
            b"auth_backends_doc_secret_32byte!".to_vec(),
        ),
        operator_secret: rustango::tenancy::session::SessionSecret::from_bytes(
            b"auth_backends_doc_operator_32by!".to_vec(),
        ),
        registry: Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });

    // `/admin` additionally needs the `post.add` permission. require_perm is
    // applied to the inner sub-router; require_auth wraps everything (outer),
    // so the user is resolved before the permission check reads it.
    let admin = Router::new()
        .route("/admin", get(admin_only))
        .require_perm("post.add");

    Router::new()
        .route("/profile", get(profile))
        .merge(admin)
        .require_auth(backends)
        .layer(axum::Extension(ctx))
}

async fn call(app: &Router, path: &str, auth: Option<&str>) -> (StatusCode, String) {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(a) = auth {
        b = b.header(header::AUTHORIZATION, a);
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn require_auth_rejects_anonymous_and_accepts_basic() {
    let (_keep, _) = setup().await;
    let app = app();

    // No credentials → 401.
    let (status, _) = call(&app, "/profile", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Correct HTTP Basic → 200, ModelBackend resolved the user.
    let (status, body) = call(&app, "/profile", Some(&basic("alice", "s3cret"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("alice"), "body: {body}");

    // Wrong password → 401 (no backend accepted; anti-enumeration).
    let (status, _) = call(&app, "/profile", Some(&basic("alice", "wrong"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_key_backend_authenticates_bearer() {
    let (pool, alice_id) = setup().await;
    // Issue a key for alice — the plaintext token is returned once.
    let token = create_api_key(alice_id, "ci-key", None, &pool)
        .await
        .expect("create_api_key");
    let app = app();
    let _keep = pool;

    let (status, body) = call(&app, "/profile", Some(&format!("Bearer {token}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("alice"), "body: {body}");

    // A bogus bearer token → 401.
    let (status, _) = call(&app, "/profile", Some("Bearer abcd1234.not-a-real-secret")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn require_perm_gates_by_codename() {
    let (_keep, _) = setup().await;
    let app = app();

    // alice has `post.add` → 200.
    let (status, _) = call(&app, "/admin", Some(&basic("alice", "s3cret"))).await;
    assert_eq!(status, StatusCode::OK);

    // bob is authenticated but lacks the permission → 403.
    let (status, _) = call(&app, "/admin", Some(&basic("bob", "bobpw"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // anonymous → 401 (auth runs before the permission check).
    let (status, _) = call(&app, "/admin", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
