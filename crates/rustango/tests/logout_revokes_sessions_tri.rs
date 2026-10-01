//! Logout ends a user's signed sessions server-side, on every backend (#1855).

#![cfg(all(feature = "admin", feature = "tenancy", feature = "sso"))]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rustango::admin::AdminUser;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::member_auth::{decode, logout, mint_cookie};
use rustango::tenancy::session::SessionSecret;
use rustango::tenancy::User;
use rustango::tri_dialect_test;
use tower::ServiceExt;

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

/// The bare admin: a cookie from before logout is refused on every
/// device, and a login in the same second works.
async fn a_bare_admin_logout_ends_every_session(pool: &Pool) {
    let password = "admin-password-1855";
    let name = unique("adm");
    let mut admin = AdminUser::new_with_password(&name, password, true).unwrap();
    admin.insert_pool(pool).await.expect("seed admin");
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    let send = |method: &'static str, uri: &'static str, body: String, cookie: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                    .header("x-csrf-token", "t")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let login = || async {
        let res = send(
            "POST",
            "/login",
            format!("username={name}&password={password}&_csrf=t"),
            String::new(),
        )
        .await;
        set_cookie(&res, "rustango_admin_session")
    };
    let status = |cookie: String| async { send("GET", "/", String::new(), cookie).await.status() };

    let laptop = login().await;
    let phone = login().await;
    assert_eq!(status(laptop.clone()).await, StatusCode::OK);

    send("POST", "/logout", "_csrf=t".into(), laptop.clone()).await;
    for (device, cookie) in [("laptop", laptop), ("phone", phone)] {
        assert!(
            status(cookie).await.is_redirection(),
            "the {device} cookie from before logout must be refused"
        );
    }
    assert_eq!(
        status(login().await).await,
        StatusCode::OK,
        "a login right after logout must work"
    );
}

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
        a_member_logout_cutoff_round_trips,
    ],
}
