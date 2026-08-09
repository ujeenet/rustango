#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds.
//! The whole ownership path, end to end: a Bearer token arrives, the tenancy
//! middleware turns it into a `Principal`, and `OwnedBy` narrows the SQL.
//!
//! Every piece has its own test elsewhere; this one exists because the seam
//! between them is where a per-user API actually leaks. It asserts the two
//! properties that matter and cannot be checked in isolation:
//!
//! * a token for member A never returns member B's rows, on **any** action;
//! * a token minted for another tenant is refused even though both tenants
//!   share one signing key.

// `not(postgres)`: the `Tenant` extractor looks up `TenantContext<DefaultDb>`,
// and enabling the postgres feature makes that Postgres — the sqlite context
// this test hands it would simply not be found.
#![cfg(all(
    feature = "sqlite",
    feature = "tenancy",
    feature = "passwords",
    feature = "admin",
    not(feature = "postgres")
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use rustango::core::Model as _;
use rustango::extractors::{Tenant, TenantContext};
use rustango::sql::sqlx;
use rustango::sql::{Auto, Pool};
use rustango::tenancy::auth_routes::require_bearer;
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use rustango::tenancy::{
    session::SessionSecret, ChainResolver, Org, OrgResolver, TenancyError, TenantPools,
};
use rustango::viewset::{OwnedBy, ViewSet};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

/// Must match what `auth_routes` signs with — the middleware verifies through
/// the same process-wide handle the login route mints from, and that handle
/// reads `RUSTANGO_SESSION_SECRET`.
const SECRET: &[u8] = b"owned_by_bearer_test_secret_32byte!!";

/// Set the signing key before anything touches the JWT handle. It is a
/// `OnceLock` inside `auth_routes`, so the first call in the process wins —
/// every test has to go through here first.
fn install_secret() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var(
            "RUSTANGO_SESSION_SECRET",
            std::str::from_utf8(SECRET).expect("utf8 secret"),
        );
    });
}

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "ob_note")]
#[rustango(app = "ob_app")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The ownership column `OwnedBy` is pointed at. Named `member_id` rather
    /// than `owner_id` on purpose: the backend must not assume a convention.
    pub member_id: i64,
    #[rustango(max_length = 200)]
    pub body: String,
}

#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait::async_trait]
impl OrgResolver for FixedResolver {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &Pool,
    ) -> Result<Option<Org>, TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

/// Each test gets its own database file, so the parallel harness doesn't have
/// them seeding over each other.
///
/// A file, not `:memory:`: the test seeds through one pool and the tenant
/// extractor opens another, and two connections to a sqlite in-memory URL are
/// two different databases — the seeded tables simply aren't there.
fn db_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rustango_owned_by_{name}.sqlite"))
}

fn db_url(name: &str) -> String {
    format!("sqlite://{}?mode=rwc", db_path(name).display())
}

fn org(slug: &str, name: &str) -> Org {
    Org {
        slug: slug.to_owned(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(db_url(name)),
        ..rustango::testkit::org()
    }
}

async fn seed_member(pool: &Pool, username: &str, active: bool) -> i64 {
    let mut u = rustango::tenancy::User {
        username: username.into(),
        password_hash: rustango::tenancy::password::hash("irrelevant").expect("hash"),
        active,
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed user");
    u.id.get().copied().expect("user id")
}

/// The app under test: notes scoped to their owner, behind Bearer auth.
///
/// `name` keys this test's private database; `slug` is the tenant the request
/// resolves to.
async fn app(slug: &str, name: &str) -> (Router, i64, i64, i64) {
    install_secret();
    // Fresh every run — a leftover file from a previous run would carry its
    // rows into this one and quietly change what the assertions mean.
    let _ = std::fs::remove_file(db_path(name));
    let seeder = sqlx::SqlitePool::connect(&db_url(name))
        .await
        .expect("tenant db");
    let pool = Pool::Sqlite(seeder);
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("users table");
    rustango::testkit::create_tables_for::<Note>(&pool)
        .await
        .expect("notes table");

    let alice = seed_member(&pool, "alice", true).await;
    let bob = seed_member(&pool, "bob", true).await;
    let ghost = seed_member(&pool, "ghost", false).await; // deactivated

    for (member_id, body) in [(alice, "alice one"), (alice, "alice two"), (bob, "bob one")] {
        let mut n = Note {
            id: Auto::Unset,
            member_id,
            body: body.into(),
        };
        n.insert_pool(&pool).await.expect("seed note");
    }

    let registry = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry");
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: Arc::new(TenantPools::new(registry)),
        resolver: ChainResolver::new().push(FixedResolver(org(slug, name))),
        session_secret: SessionSecret::from_bytes(SECRET.to_vec()),
        operator_secret: SessionSecret::from_bytes(SECRET.to_vec()),
    });

    let router = ViewSet::for_model(Note::SCHEMA)
        .page_size(100)
        .filter_backend(OwnedBy::column("member_id"))
        .tenant_router("/notes")
        .layer(axum::middleware::from_fn(require_bearer))
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));

    (router, alice, bob, ghost)
}

/// An access token exactly as `/api/auth/login` mints one: tenant-pinned.
fn token_for(user_id: i64, tenant: &str) -> String {
    let jwt = JwtLifecycle::new(SECRET.to_vec());
    let mut custom = serde_json::Map::new();
    custom.insert("tenant".into(), Value::String(tenant.to_owned()));
    jwt.issue_pair_with(user_id, custom).expect("issue").access
}

fn req(method: Method, uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).expect("request")
}

async fn json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn a_token_sees_only_its_own_rows() {
    let (app, alice, bob, _) = app("acme", "own_rows").await;

    let body = json(
        app.clone()
            .oneshot(req(Method::GET, "/notes", Some(&token_for(alice, "acme"))))
            .await
            .expect("response"),
    )
    .await;
    assert_eq!(body["count"], 2);
    for row in body["results"].as_array().expect("results") {
        assert_eq!(row["member_id"], alice);
    }

    let body = json(
        app.oneshot(req(Method::GET, "/notes", Some(&token_for(bob, "acme"))))
            .await
            .expect("response"),
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["results"][0]["body"], "bob one");
}

#[tokio::test]
async fn another_members_row_is_not_found_by_id() {
    let (app, alice, bob, _) = app("acme", "by_id").await;

    // Bob reads his own row (id 3) …
    let resp = app
        .clone()
        .oneshot(req(Method::GET, "/notes/3", Some(&token_for(bob, "acme"))))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // … and for Alice it does not exist. 404, not 403: a 403 would confirm it.
    let resp = app
        .clone()
        .oneshot(req(
            Method::GET,
            "/notes/3",
            Some(&token_for(alice, "acme")),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Deleting it is refused the same way, and the row survives.
    let resp = app
        .clone()
        .oneshot(req(
            Method::DELETE,
            "/notes/3",
            Some(&token_for(alice, "acme")),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(req(Method::GET, "/notes/3", Some(&token_for(bob, "acme"))))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_token_from_another_tenant_is_refused() {
    // Both tenants sign with the same key — the `tenant` claim is the only
    // thing that makes `sub: 1` mean a different person on each subdomain.
    let (app, alice, _, _) = app("acme", "cross_tenant").await;
    let resp = app
        .oneshot(req(
            Method::GET,
            "/notes",
            Some(&token_for(alice, "globex")),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_deactivated_account_stops_working_immediately() {
    // The token is still cryptographically valid; the account is not. The
    // middleware re-reads the row per request precisely so this does not wait
    // for the access token to expire.
    let (app, _, _, ghost) = app("acme", "deactivated").await;
    let resp = app
        .oneshot(req(Method::GET, "/notes", Some(&token_for(ghost, "acme"))))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn no_token_is_401_and_never_reaches_the_query() {
    let (app, _, _, _) = app("acme", "no_token").await;
    let resp = app
        .clone()
        .oneshot(req(Method::GET, "/notes", None))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .oneshot(req(Method::GET, "/notes", Some("not-a-token")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
