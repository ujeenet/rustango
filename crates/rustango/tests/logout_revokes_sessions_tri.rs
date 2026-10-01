//! Logout ends a user's signed sessions server-side, on every backend (#1855).

#![cfg(all(feature = "admin", feature = "tenancy", feature = "sso"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use base64::Engine as _;
use rustango::admin::AdminUser;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::tenancy::member_auth::{decode, logout, mint_cookie};
use rustango::tenancy::session::SessionSecret;
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, tenant_console, ChainResolver, Operator, Org,
    SubdomainResolver, TenantPoolInvalidator, TenantPools, User,
};
use rustango::tri_dialect_test;
use tower::ServiceExt;

type Ts = chrono::DateTime<chrono::Utc>;

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    // The login reads the TOTP device table when `totp` is on.
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

/// A whole-second cut-off a minute ahead, so no login can land after it by luck.
fn future_cutoff() -> Ts {
    chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + 60, 0).unwrap()
}

/// The `name=value` part of the response's `Set-Cookie` for `name`.
fn set_cookie(res: &axum::response::Response, name: &str) -> String {
    res.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} cookie set: {:?}", res.headers()))
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn value(cookie: &str) -> &str {
    cookie.split_once('=').unwrap().1
}

/// A form request with the double-submit CSRF pair set.
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    host: &str,
    body: String,
    cookie: &str,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, host)
                .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                .header("x-csrf-token", "t")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

// ---- bare admin ---------------------------------------------------------

const ADMIN_COOKIE: &str = "rustango_admin_session";
const ADMIN_PASSWORD: &str = "admin-password-1855";

struct Bare {
    app: Router,
    pool: Pool,
    admin: AdminUser,
}

async fn bare(pool: &Pool) -> Bare {
    let mut admin = AdminUser::new_with_password(&unique("adm"), ADMIN_PASSWORD, true).unwrap();
    admin.insert_pool(pool).await.expect("seed admin");
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    Bare {
        app,
        pool: pool.clone(),
        admin,
    }
}

impl Bare {
    async fn login(&self) -> String {
        let form = format!(
            "username={}&password={ADMIN_PASSWORD}&_csrf=t",
            self.admin.username
        );
        set_cookie(
            &send(&self.app, "POST", "/login", "admin.test", form, "").await,
            ADMIN_COOKIE,
        )
    }

    async fn status(&self, cookie: &str) -> StatusCode {
        send(&self.app, "GET", "/", "admin.test", String::new(), cookie)
            .await
            .status()
    }

    async fn logout(&self, cookie: &str) -> StatusCode {
        send(
            &self.app,
            "POST",
            "/logout",
            "admin.test",
            "_csrf=t".into(),
            cookie,
        )
        .await
        .status()
    }

    async fn row(&self) -> AdminUser {
        AdminUser::objects()
            .filter("id", self.admin.id.get().copied().unwrap())
            .fetch(&self.pool)
            .await
            .unwrap()
            .remove(0)
    }

    async fn set_cutoff(&self, at: Option<Ts>) {
        let mut row = self.row().await;
        row.sessions_revoked_at = at;
        row.save_pool(&self.pool).await.expect("stamp cut-off");
    }
}

/// The `iat` of a bare-admin cookie (`base64(json).sig`).
fn admin_iat(cookie: &str) -> i64 {
    let body = value(cookie).split_once('.').unwrap().0;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&json).unwrap()["iat"]
        .as_i64()
        .unwrap()
}

/// A cookie from before logout is refused on every device.
async fn a_bare_admin_logout_ends_every_session(pool: &Pool) {
    let env = bare(pool).await;
    let laptop = env.login().await;
    let phone = env.login().await;
    assert_eq!(env.status(&laptop).await, StatusCode::OK);

    assert!(env.logout(&laptop).await.is_redirection());
    for (device, cookie) in [("laptop", laptop), ("phone", phone)] {
        assert!(
            env.status(&cookie).await.is_redirection(),
            "the {device} cookie from before logout must be refused"
        );
    }
}

/// A login gets `iat` = cut-off + 1, so it works right after a logout.
async fn a_bare_admin_login_lands_after_the_cutoff(pool: &Pool) {
    let env = bare(pool).await;
    let cut = future_cutoff();
    env.set_cutoff(Some(cut)).await;
    let cookie = env.login().await;
    assert_eq!(admin_iat(&cookie), cut.timestamp() + 1);
    assert_eq!(env.status(&cookie).await, StatusCode::OK);
}

/// A cookie whose `iat` is ahead of this node's clock still ends at logout.
async fn a_bare_admin_logout_ends_a_cookie_from_a_faster_clock(pool: &Pool) {
    let env = bare(pool).await;
    env.set_cutoff(Some(future_cutoff())).await;
    let cookie = env.login().await;
    env.set_cutoff(None).await;
    assert_eq!(env.status(&cookie).await, StatusCode::OK);

    assert!(env.logout(&cookie).await.is_redirection());
    let cut = env.row().await.sessions_revoked_at.expect("stamped");
    assert_eq!(cut.timestamp(), admin_iat(&cookie) + 1);
    assert!(env.status(&cookie).await.is_redirection());
}

/// A signed but stale cookie logs this browser out and revokes nothing.
async fn a_stale_bare_admin_cookie_cannot_revoke(pool: &Pool) {
    let env = bare(pool).await;
    let cookie = env.login().await;
    let mut row = env.row().await;
    row.password_hash = "$argon2id$changed".into();
    row.save_pool(&env.pool).await.unwrap();

    assert!(env.logout(&cookie).await.is_redirection());
    assert_eq!(env.row().await.sessions_revoked_at, None);
}

/// A logout that cannot read the user is a 500, not a fake success.
async fn a_bare_admin_logout_that_cannot_read_fails(pool: &Pool) {
    let env = bare(pool).await;
    let cookie = env.login().await;
    pool.close().await;
    assert_eq!(env.logout(&cookie).await, StatusCode::INTERNAL_SERVER_ERROR);
}

// ---- tenant admin + operator console -----------------------------------

const TENANT_PASSWORD: &str = "tenant-password-1855";
const OPERATOR_PASSWORD: &str = "operator-password-1855";
const OP_COOKIE: &str = "rustango_op_session";

struct Tenancy {
    admin: Router,
    console: Router,
    /// Registry and tenant data: one database.
    db: Pool,
    /// The tenant admin's cached tenant pool.
    tenant_pool: Pool,
    /// The console's own registry pool.
    console_pool: Pool,
    secret: SessionSecret,
    op_secret: SessionSecret,
    slug: String,
    host: String,
    _dir: tempfile::TempDir,
}

/// Wire the tenant admin over `pools`; also returns its cached tenant pool.
async fn wire<DB: sqlx::Database>(
    pools: Arc<TenantPools<DB>>,
    url: &str,
    org: &Org,
    secret: &SessionSecret,
) -> (Router, Arc<dyn TenantPoolInvalidator>, Pool)
where
    Pool: From<sqlx::Pool<DB>>,
{
    let tenant_pool = pools.scoped_pool_dyn(org).await.expect("tenant pool");
    let admin = TenantAdminBuilder::new(
        pools.clone(),
        url,
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy())
    .with_session(secret.clone())
    .build();
    (admin, pools, tenant_pool)
}

/// One database-mode tenant on `pool`'s backend, its admin and the console.
async fn tenancy(pool: &Pool) -> Tenancy {
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

    let slug = unique("t");
    let host = format!("{slug}.app.test");
    let mut org = Org {
        slug: slug.clone(),
        storage_mode: "database".into(),
        backend_kind: kind.into(),
        database_url: Some(url.clone()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&db).await.expect("seed org");

    let secret = SessionSecret::from_bytes(b"tenant-admin-session-secret-32b!".to_vec());
    let op_secret = SessionSecret::from_bytes(b"operator-console-secret-32bytes!".to_vec());
    let (admin, invalidator, tenant_pool) = match &db {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => {
            wire(
                Arc::new(TenantPools::<sqlx::Postgres>::new(p.clone())),
                &url,
                &org,
                &secret,
            )
            .await
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => {
            wire(
                Arc::new(TenantPools::<sqlx::MySql>::new(p.clone())),
                &url,
                &org,
                &secret,
            )
            .await
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => {
            wire(
                Arc::new(TenantPools::<sqlx::Sqlite>::new(p.clone())),
                &url,
                &org,
                &secret,
            )
            .await
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("unknown backend"),
    };
    let console_pool = Pool::connect(&url).await.unwrap();
    let console = rustango::tenancy::operator_console::router_with_impersonation(
        console_pool.clone(),
        invalidator,
        op_secret.clone(),
        Arc::new(rustango::storage::LocalStorage::new(
            dir.path().join("brand"),
        )),
        secret.clone(),
        RouteConfig::legacy().impersonation_handoff_url,
    );
    Tenancy {
        admin,
        console,
        db,
        tenant_pool,
        console_pool,
        secret,
        op_secret,
        slug,
        host,
        _dir: dir,
    }
}

impl Tenancy {
    async fn admin_req(
        &self,
        method: &str,
        uri: &str,
        body: String,
        cookie: &str,
    ) -> axum::response::Response {
        send(&self.admin, method, uri, &self.host, body, cookie).await
    }

    async fn console_req(
        &self,
        method: &str,
        uri: &str,
        body: String,
        cookie: &str,
    ) -> axum::response::Response {
        send(&self.console, method, uri, "console.test", body, cookie).await
    }

    async fn user(&self) -> User {
        let mut user = User {
            username: unique("u"),
            is_superuser: true,
            password_hash: rustango::tenancy::password::hash(TENANT_PASSWORD).unwrap(),
            ..rustango::testkit::user()
        };
        user.insert_pool(&self.db).await.expect("seed user");
        user
    }

    async fn user_row(&self, user: &User) -> User {
        User::objects()
            .filter("id", user.id.get().copied().unwrap())
            .fetch(&self.db)
            .await
            .unwrap()
            .remove(0)
    }

    async fn set_user_cutoff(&self, user: &User, at: Option<Ts>) {
        let mut row = self.user_row(user).await;
        row.sessions_revoked_at = at;
        row.save_pool(&self.db).await.expect("stamp cut-off");
    }

    async fn login(&self, user: &User) -> String {
        let form = format!(
            "username={}&password={TENANT_PASSWORD}&_csrf=t",
            user.username
        );
        let res = self.admin_req("POST", "/__login", form, "").await;
        set_cookie(&res, tenant_console::COOKIE_NAME)
    }

    async fn tenant_status(&self, cookie: &str) -> StatusCode {
        self.admin_req("GET", "/__change-password", String::new(), cookie)
            .await
            .status()
    }

    async fn tenant_logout(&self, cookie: &str) -> StatusCode {
        self.admin_req("POST", "/__logout", "_csrf=t".into(), cookie)
            .await
            .status()
    }

    fn tenant_iat(&self, cookie: &str) -> i64 {
        tenant_console::decode(&self.secret, &self.slug, value(cookie))
            .unwrap()
            .iat
    }

    async fn operator(&self) -> Operator {
        let mut op = Operator {
            id: Auto::default(),
            username: unique("op"),
            password_hash: rustango::tenancy::password::hash(OPERATOR_PASSWORD).unwrap(),
            active: true,
            created_at: chrono::Utc::now(),
            password_changed_at: None,
            sessions_revoked_at: None,
        };
        op.insert_pool(&self.db).await.expect("seed operator");
        op
    }

    async fn op_row(&self, op: &Operator) -> Operator {
        Operator::objects()
            .filter("id", op.id.get().copied().unwrap())
            .fetch(&self.db)
            .await
            .unwrap()
            .remove(0)
    }

    async fn set_op_cutoff(&self, op: &Operator, at: Option<Ts>) {
        let mut row = self.op_row(op).await;
        row.sessions_revoked_at = at;
        row.save_pool(&self.db).await.expect("stamp cut-off");
    }

    async fn console_login(&self, op: &Operator) -> String {
        let form = format!("username={}&password={OPERATOR_PASSWORD}", op.username);
        set_cookie(
            &self.console_req("POST", "/login", form, "").await,
            OP_COOKIE,
        )
    }

    async fn console_status(&self, cookie: &str) -> StatusCode {
        self.console_req("GET", "/", String::new(), cookie)
            .await
            .status()
    }

    async fn console_logout(&self, cookie: &str) -> StatusCode {
        self.console_req("POST", "/logout", String::new(), cookie)
            .await
            .status()
    }

    fn console_iat(&self, cookie: &str) -> i64 {
        rustango::tenancy::session::decode(&self.op_secret, value(cookie))
            .unwrap()
            .iat
    }

    /// Start an impersonation from the console; the handoff URL path.
    async fn handoff(&self, op_cookie: &str) -> String {
        let start = self
            .console_req(
                "POST",
                &format!("/orgs/{}/impersonate", self.slug),
                String::new(),
                op_cookie,
            )
            .await;
        let location = start.headers()["location"].to_str().unwrap().to_owned();
        location[location
            .find("/__impersonation_handoff")
            .expect("handoff url")..]
            .to_owned()
    }

    async fn redeem(&self, handoff: &str) -> axum::response::Response {
        self.admin_req("GET", handoff, String::new(), "").await
    }
}

/// Logout ends the tenant session on every device.
async fn a_tenant_admin_logout_ends_every_session(pool: &Pool) {
    let env = tenancy(pool).await;
    let user = env.user().await;
    let laptop = env.login(&user).await;
    let phone = env.login(&user).await;
    assert_eq!(env.tenant_status(&laptop).await, StatusCode::OK);

    assert!(env.tenant_logout(&laptop).await.is_redirection());
    for (device, cookie) in [("laptop", &laptop), ("phone", &phone)] {
        assert_eq!(
            env.tenant_status(cookie).await,
            StatusCode::SEE_OTHER,
            "the {device} cookie from before logout must be refused"
        );
    }
}

/// A tenant login gets `iat` = cut-off + 1.
async fn a_tenant_admin_login_lands_after_the_cutoff(pool: &Pool) {
    let env = tenancy(pool).await;
    let user = env.user().await;
    let cut = future_cutoff();
    env.set_user_cutoff(&user, Some(cut)).await;
    let cookie = env.login(&user).await;
    assert_eq!(env.tenant_iat(&cookie), cut.timestamp() + 1);
    assert_eq!(env.tenant_status(&cookie).await, StatusCode::OK);
}

/// A tenant cookie whose `iat` is ahead of this node's clock ends at logout.
async fn a_tenant_admin_logout_ends_a_cookie_from_a_faster_clock(pool: &Pool) {
    let env = tenancy(pool).await;
    let user = env.user().await;
    env.set_user_cutoff(&user, Some(future_cutoff())).await;
    let cookie = env.login(&user).await;
    env.set_user_cutoff(&user, None).await;
    assert_eq!(env.tenant_status(&cookie).await, StatusCode::OK);

    assert!(env.tenant_logout(&cookie).await.is_redirection());
    let cut = env.user_row(&user).await.sessions_revoked_at.unwrap();
    assert_eq!(cut.timestamp(), env.tenant_iat(&cookie) + 1);
    assert_eq!(env.tenant_status(&cookie).await, StatusCode::SEE_OTHER);
}

/// A signed but stale tenant cookie revokes nothing.
async fn a_stale_tenant_admin_cookie_cannot_revoke(pool: &Pool) {
    let env = tenancy(pool).await;
    let user = env.user().await;
    let cookie = env.login(&user).await;
    let mut row = env.user_row(&user).await;
    row.password_hash = "$argon2id$changed".into();
    row.save_pool(&env.db).await.unwrap();

    assert!(env.tenant_logout(&cookie).await.is_redirection());
    assert_eq!(env.user_row(&user).await.sessions_revoked_at, None);
}

/// A tenant logout that cannot read the user is a 500.
async fn a_tenant_admin_logout_that_cannot_read_fails(pool: &Pool) {
    let env = tenancy(pool).await;
    let user = env.user().await;
    let cookie = env.login(&user).await;
    env.tenant_pool.close().await;
    assert_eq!(
        env.tenant_logout(&cookie).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

/// Logging out of an impersonation leaves the operator signed in; the
/// console logout then ends both.
async fn an_impersonation_logout_keeps_the_operator_signed_in(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    let op_cookie = env.console_login(&op).await;
    let imp = set_cookie(
        &env.redeem(&env.handoff(&op_cookie).await).await,
        tenant_console::COOKIE_NAME,
    );
    assert_eq!(env.tenant_status(&imp).await, StatusCode::OK);

    assert!(env.tenant_logout(&imp).await.is_redirection());
    assert_eq!(env.op_row(&op).await.sessions_revoked_at, None);
    assert_eq!(
        env.console_status(&op_cookie).await,
        StatusCode::OK,
        "an impersonation logout must not sign the operator out"
    );

    let imp = set_cookie(
        &env.redeem(&env.handoff(&op_cookie).await).await,
        tenant_console::COOKIE_NAME,
    );
    assert!(env.console_logout(&op_cookie).await.is_redirection());
    assert_eq!(env.console_status(&op_cookie).await, StatusCode::SEE_OTHER);
    assert_eq!(
        env.tenant_status(&imp).await,
        StatusCode::SEE_OTHER,
        "an impersonation cookie must end with the operator's logout"
    );
}

/// A handoff minted before the operator logged out is refused.
async fn a_handoff_from_before_the_operator_logout_is_refused(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    let op_cookie = env.console_login(&op).await;
    let handoff = env.handoff(&op_cookie).await;
    assert!(env.console_logout(&op_cookie).await.is_redirection());
    assert_eq!(
        env.redeem(&handoff).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

/// The impersonation session dates from the handoff's mint: `iat` = the
/// operator's cut-off + 1, so it works right after a logout.
async fn an_impersonation_session_carries_the_handoff_mint_time(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    let cut = future_cutoff();
    env.set_op_cutoff(&op, Some(cut)).await;
    let op_cookie = env.console_login(&op).await;
    let imp = set_cookie(
        &env.redeem(&env.handoff(&op_cookie).await).await,
        tenant_console::COOKIE_NAME,
    );
    assert_eq!(env.tenant_iat(&imp), cut.timestamp() + 1);
    assert_eq!(env.tenant_status(&imp).await, StatusCode::OK);
}

/// A console login gets `iat` = cut-off + 1.
async fn a_console_login_lands_after_the_cutoff(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    let cut = future_cutoff();
    env.set_op_cutoff(&op, Some(cut)).await;
    let cookie = env.console_login(&op).await;
    assert_eq!(env.console_iat(&cookie), cut.timestamp() + 1);
    assert_eq!(env.console_status(&cookie).await, StatusCode::OK);
}

/// A console cookie whose `iat` is ahead of this node's clock ends at logout.
async fn a_console_logout_ends_a_cookie_from_a_faster_clock(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    env.set_op_cutoff(&op, Some(future_cutoff())).await;
    let cookie = env.console_login(&op).await;
    env.set_op_cutoff(&op, None).await;
    assert_eq!(env.console_status(&cookie).await, StatusCode::OK);

    assert!(env.console_logout(&cookie).await.is_redirection());
    let cut = env.op_row(&op).await.sessions_revoked_at.unwrap();
    assert_eq!(cut.timestamp(), env.console_iat(&cookie) + 1);
    assert_eq!(env.console_status(&cookie).await, StatusCode::SEE_OTHER);
}

/// A signed but stale console cookie revokes nothing.
async fn a_stale_console_cookie_cannot_revoke(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    let cookie = env.console_login(&op).await;
    let mut row = env.op_row(&op).await;
    row.password_hash = "$argon2id$changed".into();
    row.save_pool(&env.db).await.unwrap();

    assert!(env.console_logout(&cookie).await.is_redirection());
    assert_eq!(env.op_row(&op).await.sessions_revoked_at, None);
}

/// A console logout that cannot read the operator is a 500.
async fn a_console_logout_that_cannot_read_fails(pool: &Pool) {
    let env = tenancy(pool).await;
    let op = env.operator().await;
    let cookie = env.console_login(&op).await;
    env.console_pool.close().await;
    assert_eq!(
        env.console_logout(&cookie).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

// ---- member -------------------------------------------------------------

/// The tenant user's cut-off: stored whole-second, never moved back, and
/// always before the next login's `iat`.
async fn a_member_logout_cutoff_round_trips(pool: &Pool) {
    let secret = SessionSecret::from_bytes(vec![9u8; 32]);
    let mut user = User {
        username: unique("m"),
        ..rustango::testkit::user()
    };
    user.insert_pool(pool).await.expect("seed user");
    let reload = || async {
        User::objects()
            .filter("id", user.id.get().copied().unwrap())
            .fetch(pool)
            .await
            .unwrap()
            .remove(0)
    };
    let iat = |cookie: String| {
        let value = cookie
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_owned();
        decode(&secret, "acme", &value).unwrap().iat
    };

    let before = iat(mint_cookie(&secret, &user, "acme", 3600));
    logout(pool, &user).await.expect("logout");
    let user = reload().await;
    let cut = user.sessions_revoked_at.expect("stamped").timestamp();
    assert!(before <= cut, "the earlier session is covered");

    let next = iat(mint_cookie(&secret, &user, "acme", 3600));
    assert!(next > cut, "the next login lands after the cut-off");

    // A second logout in the same second still covers that login.
    logout(pool, &user).await.expect("logout again");
    let user = reload().await;
    assert!(next <= user.sessions_revoked_at.unwrap().timestamp());
    // A stale row cannot move the cut-off back.
    let mut stale = user.clone();
    stale.sessions_revoked_at = None;
    logout(pool, &stale).await.expect("stale logout");
    assert!(reload().await.sessions_revoked_at >= user.sessions_revoked_at);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        a_bare_admin_logout_ends_every_session,
        a_bare_admin_login_lands_after_the_cutoff,
        a_bare_admin_logout_ends_a_cookie_from_a_faster_clock,
        a_stale_bare_admin_cookie_cannot_revoke,
        a_bare_admin_logout_that_cannot_read_fails,
        a_tenant_admin_logout_ends_every_session,
        a_tenant_admin_login_lands_after_the_cutoff,
        a_tenant_admin_logout_ends_a_cookie_from_a_faster_clock,
        a_stale_tenant_admin_cookie_cannot_revoke,
        a_tenant_admin_logout_that_cannot_read_fails,
        an_impersonation_logout_keeps_the_operator_signed_in,
        a_handoff_from_before_the_operator_logout_is_refused,
        an_impersonation_session_carries_the_handoff_mint_time,
        a_console_login_lands_after_the_cutoff,
        a_console_logout_ends_a_cookie_from_a_faster_clock,
        a_stale_console_cookie_cannot_revoke,
        a_console_logout_that_cannot_read_fails,
        a_member_logout_cutoff_round_trips,
    ],
}
