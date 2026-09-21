//! A credential must not authenticate on another tenant's host.
//!
//! `require_auth` used to take a `Pool` captured when the router was
//! built, so every request authenticated against that one database
//! whatever host it arrived on.
//!
//! Two tenants, two SQLite databases, one router. The credential exists
//! in `alpha` only.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::HOST;
use axum::http::{header, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::DatabaseTenantContext;
use rustango::sql::sqlx;
use rustango::tenancy::auth_backends::{AuthBackend, ModelBackend};
use rustango::tenancy::{
    session::SessionSecret, BackendKind, ChainResolver, CurrentUser, DatabasePools, Org,
    OrgResolver, RouterAuthExt, TenancyError,
};
use tower::ServiceExt as _;

/// `Host: <slug>.localhost` → synthetic Org, database-mode SQLite.
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
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let slug = host.split('.').next().unwrap_or("");
        if slug.is_empty() || !host.contains('.') {
            return Ok(None);
        }
        Ok(Some(Org {
            id: rustango::sql::Auto::default(),
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: None,
            ..rustango::testkit::org()
        }))
    }
}

async fn whoami(CurrentUser(user): CurrentUser) -> impl IntoResponse {
    match user {
        Some(u) => (StatusCode::OK, u.username).into_response(),
        None => (StatusCode::UNAUTHORIZED, "anonymous").into_response(),
    }
}

/// Minimal base64 for the Basic header — no extra dependency.
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

/// Build a tenant database with the users table, optionally holding
/// one user. Returns the pool so the caller can keep the `cache=shared`
/// in-memory database alive for the duration of the test.
async fn tenant_db(url: &str, user: Option<(&str, &str)>) -> rustango::sql::Pool {
    let pool = rustango::sql::Pool::connect(url).await.expect("tenant db");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("users table");
    if let Some((username, password)) = user {
        let hash = rustango::tenancy::password::hash(password).expect("hash");
        let rustango::sql::Pool::Sqlite(sq) = &pool else {
            unreachable!("sqlite-only test")
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
    pool
}

fn build_app(ns: &str) -> (Router, String) {
    let template = format!("sqlite:file:xtenant_{ns}_{{slug}}?mode=memory&cache=shared");
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver: ChainResolver::new().push(HostResolver),
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite"),
        ),
    });

    let backends: Vec<Arc<dyn AuthBackend>> = vec![Arc::new(ModelBackend)];
    let app = Router::new()
        .route("/whoami", get(whoami))
        .require_auth(backends)
        .layer(axum::Extension(ctx));
    (app, template)
}

async fn call(app: &Router, host: &str, basic: &str) -> StatusCode {
    let req = Request::builder()
        .uri("/whoami")
        .header(HOST, host)
        .header(header::AUTHORIZATION, format!("Basic {}", b64(basic)))
        .body(Body::empty())
        .expect("request");
    app.clone().oneshot(req).await.expect("response").status()
}

/// The credential works on its own tenant — the control. Without this
/// the rejection below could be "authentication is broken everywhere",
/// which would pass the security assertion for the wrong reason.
#[tokio::test]
async fn the_credential_works_on_its_own_tenant() {
    let (app, template) = build_app("own");
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;

    assert_eq!(
        call(&app, "alpha.localhost", "ann:s3cret").await,
        StatusCode::OK,
        "ann exists in alpha and must authenticate there"
    );
}

/// The same credential must be refused on another tenant's host.
///
/// This is the vulnerability. With the pool captured at mount time the
/// backend looked `ann` up in whichever database the router was built
/// with and admitted her to `beta` — where she has no row at all.
#[tokio::test]
async fn the_same_credential_is_refused_on_another_tenants_host() {
    let (app, template) = build_app("cross");
    let _alpha = tenant_db(
        &template.replace("{slug}", "alpha"),
        Some(("ann", "s3cret")),
    )
    .await;
    // `beta` exists and is reachable, but holds no `ann`. Creating its
    // table matters: a database with no table at all could be refused
    // for the wrong reason, which would pass this test while the bug
    // was fully present.
    let _beta = tenant_db(&template.replace("{slug}", "beta"), None).await;

    assert_eq!(
        call(&app, "beta.localhost", "ann:s3cret").await,
        StatusCode::UNAUTHORIZED,
        "ann has no row in beta, so her credential must not authenticate \
         there. A 200 means the middleware checked her against some other \
         tenant's database — the cross-tenant authentication bypass."
    );
}
