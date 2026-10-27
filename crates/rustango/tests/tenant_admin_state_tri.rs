//! The tenant admin builds its routes once and carries each request's
//! user in that request: perms never carry over to the next request.
//! It also refuses a tenant user's own deactivation, on every backend.

#![cfg(all(feature = "admin", feature = "tenancy", feature = "testkit"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rustango::sql::{sqlx, FetcherPool as _, Pool};
use rustango::tenancy::session::SessionSecret;
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, tenant_console, ChainResolver, Org,
    SubdomainResolver, TenantPools, User,
};
use rustango::tri_dialect_test;
use tower::ServiceExt;

const PASSWORD: &str = "tenant-admin-state-pw";

/// Router builds are counted process-wide; scenarios on other backends
/// must not build one mid-count.
static BUILDS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(pool)
        .await
        .expect("totp table");
}

/// Unique per run: the shared tables keep earlier runs' rows.
fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

struct Tenant {
    admin: Router,
    db: Pool,
    host: String,
    _dir: tempfile::TempDir,
}

fn wire<DB: sqlx::Database>(pools: Arc<TenantPools<DB>>, url: &str, session: bool) -> Router
where
    Pool: From<sqlx::Pool<DB>>,
{
    let b = TenantAdminBuilder::new(
        pools,
        url,
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy());
    if session {
        b.with_session(SessionSecret::from_bytes(
            b"tenant-admin-state-secret-32byte".to_vec(),
        ))
        .build()
    } else {
        b.build()
    }
}

/// One database-mode tenant on `pool`'s backend and its admin.
async fn tenant(pool: &Pool, session: bool) -> Tenant {
    let dir = tempfile::tempdir().unwrap();
    let (url, kind) = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(_) => (std::env::var("DATABASE_URL").unwrap(), "postgres"),
        #[cfg(feature = "mysql")]
        Pool::Mysql(_) => (std::env::var("MYSQL_TEST_URL").unwrap(), "mysql"),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(_) => (
            format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display()),
            "sqlite",
        ),
        #[allow(unreachable_patterns)]
        _ => unreachable!("unknown backend"),
    };
    let db = Pool::connect(&url).await.unwrap();
    setup(&db).await;
    let slug = unique("st");
    let host = format!("{slug}.app.test");
    let mut org = Org {
        slug,
        storage_mode: "database".into(),
        backend_kind: kind.into(),
        database_url: Some(url.clone()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&db).await.expect("seed org");
    let admin = match &db {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => wire(
            Arc::new(TenantPools::<sqlx::Postgres>::new(p.clone())),
            &url,
            session,
        ),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => wire(
            Arc::new(TenantPools::<sqlx::MySql>::new(p.clone())),
            &url,
            session,
        ),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => wire(
            Arc::new(TenantPools::<sqlx::Sqlite>::new(p.clone())),
            &url,
            session,
        ),
        #[allow(unreachable_patterns)]
        _ => unreachable!("unknown backend"),
    };
    Tenant {
        admin,
        db,
        host,
        _dir: dir,
    }
}

impl Tenant {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        body: String,
        cookie: &str,
    ) -> (StatusCode, String, Option<String>) {
        let res = self
            .admin
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::HOST, &self.host)
                    .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                    .header("x-csrf-token", "t")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let session = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{}=", tenant_console::COOKIE_NAME)))
            .map(|v| v.split(';').next().unwrap().to_owned());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            String::from_utf8_lossy(&bytes).into_owned(),
            session,
        )
    }

    async fn user(&self, is_superuser: bool) -> User {
        let mut user = User {
            username: unique("u"),
            is_superuser,
            password_hash: rustango::tenancy::password::hash(PASSWORD).unwrap(),
            ..rustango::testkit::user()
        };
        user.insert_pool(&self.db).await.expect("seed user");
        user
    }

    async fn login(&self, user: &User) -> String {
        let form = format!("username={}&password={PASSWORD}&_csrf=t", user.username);
        let (status, body, cookie) = self.send("POST", "/__login", form, "").await;
        cookie.unwrap_or_else(|| panic!("no session cookie: {status} {body}"))
    }

    async fn active(&self, user: &User) -> bool {
        User::objects()
            .filter("id", user.id.get().copied().unwrap())
            .fetch(&self.db)
            .await
            .unwrap()
            .remove(0)
            .active
    }
}

/// Requests reuse the routes `build` made; none builds its own.
async fn requests_do_not_rebuild_the_router(pool: &Pool) {
    let _guard = BUILDS.lock().await;
    let env = tenant(pool, false).await;
    let before = rustango::admin::__router_builds();
    for uri in ["/__admin/", "/__admin/rustango_users", "/__admin/__audit"] {
        let (status, body, _) = env.send("GET", uri, String::new(), "").await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    assert_eq!(rustango::admin::__router_builds(), before);
}

/// A superuser's access does not carry over to a user with no perms on
/// the same router, in either order.
async fn perms_stay_with_their_request(pool: &Pool) {
    let _guard = BUILDS.lock().await;
    let env = tenant(pool, true).await;
    let boss = env.user(true).await;
    let clerk = env.user(false).await;
    let boss_cookie = env.login(&boss).await;
    let clerk_cookie = env.login(&clerk).await;
    let list = "/__admin/rustango_users";
    for _ in 0..2 {
        let (status, body, _) = env.send("GET", list, String::new(), &boss_cookie).await;
        assert_eq!(status, StatusCode::OK, "superuser: {body}");
        assert!(body.contains(&clerk.username), "{body}");
        let (status, body, _) = env.send("GET", list, String::new(), &clerk_cookie).await;
        assert_ne!(status, StatusCode::OK, "no-perm user: {body}");
        assert!(!body.contains(&boss.username), "{body}");
    }
}

/// The edit form for `u` with `active` unticked.
fn deactivate_form(u: &User) -> String {
    format!(
        "_csrf=t&username={}&password_hash=&is_superuser=on&created_at=2026-01-01T00:00:00&data=%7B%7D",
        u.username
    )
}

/// A tenant user cannot deactivate their own account; another one's saves.
async fn tenant_user_cannot_deactivate_themselves(pool: &Pool) {
    let _guard = BUILDS.lock().await;
    let env = tenant(pool, true).await;
    let me = env.user(true).await;
    let other = env.user(true).await;
    let cookie = env.login(&me).await;

    let uri = format!("/__admin/rustango_users/{}", me.id.get().unwrap());
    let (status, body, _) = env.send("POST", &uri, deactivate_form(&me), &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("cannot deactivate yourself"), "{body}");
    assert!(env.active(&me).await, "own account deactivated");

    let uri = format!("/__admin/rustango_users/{}", other.id.get().unwrap());
    let (status, body, _) = env
        .send("POST", &uri, deactivate_form(&other), &cookie)
        .await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(!env.active(&other).await);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        requests_do_not_rebuild_the_router,
        perms_stay_with_their_request,
        tenant_user_cannot_deactivate_themselves,
    ],
}
