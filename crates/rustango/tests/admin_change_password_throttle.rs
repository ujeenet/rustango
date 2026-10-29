//! Bare admin change-password: wrong current passwords lock the account
//! like failed logins, so a stolen session cannot guess it (#1873), and
//! the new-password length is counted in characters (#1874).

#![cfg(all(feature = "sqlite", feature = "admin", feature = "testkit"))]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rustango::admin::{AdminUser, Builder};
use rustango::session::SessionSecret;
use rustango::sql::{sqlx, Pool};
use tower::ServiceExt as _;

const CSRF: &str = "csrftokencsrftoken";

async fn post(
    app: &axum::Router,
    uri: &str,
    cookie: &str,
    body: String,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={CSRF}; {cookie}"))
                .body(Body::from(format!("_csrf={CSRF}&{body}")))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn boot(name: &str) -> (axum::Router, String) {
    let (app, session, _) = boot_with_pool(name).await;
    (app, session)
}

async fn boot_with_pool(name: &str) -> (axum::Router, String, Pool) {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let pool: Pool = p.into();
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    // With `totp` on, login reads the device table.
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(&pool)
        .await
        .unwrap();
    AdminUser::new_with_password(name, "correct-horse", true)
        .unwrap()
        .insert_pool(&pool)
        .await
        .unwrap();
    let app = Builder::new(pool.clone())
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build();

    let login = |pass: &str| format!("username={name}&password={pass}");
    let resp = post(&app, "/login", "", login("correct-horse")).await;
    let session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            s.starts_with("rustango_admin_session=")
                .then(|| s.split(';').next().unwrap_or("").to_owned())
        })
        .expect("session cookie");
    (app, session, pool)
}

#[tokio::test]
async fn change_password_misses_lock_the_account() {
    let name = format!("cpw{}", std::process::id());
    let (app, session) = boot(&name).await;
    let login = |pass: &str| format!("username={name}&password={pass}");
    let change = |current: &str| {
        format!("current_password={current}&new_password=another-pass-9&new_password_confirm=another-pass-9")
    };
    for _ in 0..5 {
        let r = post(&app, "/account/password", &session, change("wrong")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }
    let r = post(&app, "/account/password", &session, change("correct-horse")).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS, "change-password");
    let r = post(&app, "/login", "", login("correct-horse")).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS, "login");
}

/// Ten two-byte characters are twenty bytes but still too short.
#[tokio::test]
async fn new_password_length_counts_characters_not_bytes() {
    let (app, session) = boot("cpwlen").await;
    let short = "%C3%A9".repeat(10);
    let body =
        format!("current_password=correct-horse&new_password={short}&new_password_confirm={short}");
    let r = post(&app, "/account/password", &session, body).await;
    let html = String::from_utf8(
        axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains("at least 12"),
        "short password accepted: {html}"
    );
    assert!(
        html.contains(r#"minlength="12""#) && html.contains("At least 12 characters"),
        "the form hints the old length rule: {html}"
    );
}

/// A failed password write shows a generic message, not the driver error.
#[tokio::test]
async fn a_failed_update_hides_the_driver_error() {
    let (app, session, pool) = boot_with_pool("cpwdb").await;
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TRIGGER cpw_fail BEFORE UPDATE ON rustango_admin_users \
         BEGIN SELECT RAISE(ABORT, 'internal_constraint_q7Zx'); END",
        Vec::new(),
    )
    .await
    .unwrap();
    let body =
        "current_password=correct-horse&new_password=another-pass-9&new_password_confirm=another-pass-9";
    let r = post(&app, "/account/password", &session, body.to_owned()).await;
    let html = String::from_utf8(
        axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("internal_constraint_q7Zx"), "{html}");
    assert!(html.contains("Could not save"), "{html}");
}
