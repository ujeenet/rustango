//! The admin refuses an edit that deactivates the signed-in user, on every
//! backend; deactivating another admin still saves.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin",
    feature = "testkit"
))]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rustango::admin::AdminUser;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tri_dialect_test;
use tower::ServiceExt as _;

const PASSWORD: &str = "self-deactivate-pw-1";

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

/// Unique per run: the shared table keeps earlier runs' rows.
fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: String,
    cookie: &str,
) -> (StatusCode, String, Option<String>) {
    let res = app
        .clone()
        .oneshot(
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
        .unwrap();
    let status = res.status();
    let session = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("rustango_admin_session="))
        .map(|v| v.split(';').next().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        session,
    )
}

async fn seed(pool: &Pool, prefix: &str) -> AdminUser {
    let mut u = AdminUser::new_with_password(unique(prefix), PASSWORD, true).unwrap();
    u.insert_pool(pool).await.expect("seed admin");
    u
}

async fn active(pool: &Pool, username: &str) -> bool {
    AdminUser::objects()
        .fetch(pool)
        .await
        .expect("admins")
        .into_iter()
        .find(|u| u.username == username)
        .expect("admin row")
        .active
}

/// The edit form for `u` with `active` unticked.
fn deactivate_form(u: &AdminUser) -> String {
    format!(
        "_csrf=t&username={}&password_hash=&is_superuser=on",
        u.username
    )
}

async fn admin_cannot_deactivate_themselves(pool: &Pool) {
    let me = seed(pool, "me").await;
    let other = seed(pool, "other").await;
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_session_auth(rustango::session::SessionSecret::from_bytes(vec![9u8; 32]))
        .build();
    let login = format!("_csrf=t&username={}&password={PASSWORD}", me.username);
    let (status, body, cookie) = send(&app, "POST", "/login", login, "").await;
    let cookie = cookie.unwrap_or_else(|| panic!("no session cookie: {status} {body}"));

    let me_id = *me.id.get().expect("pk");
    let uri = format!("/rustango_admin_users/{me_id}");
    let (status, body, _) = send(&app, "POST", &uri, deactivate_form(&me), &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("cannot deactivate yourself"), "{body}");
    assert!(active(pool, &me.username).await, "own account deactivated");

    let other_id = *other.id.get().expect("pk");
    let uri = format!("/rustango_admin_users/{other_id}");
    let (status, body, _) = send(&app, "POST", &uri, deactivate_form(&other), &cookie).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(!active(pool, &other.username).await);
}

tri_dialect_test!(
    setup: setup,
    scenarios: [admin_cannot_deactivate_themselves],
);
