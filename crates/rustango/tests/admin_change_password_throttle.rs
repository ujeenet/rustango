//! Bare admin change-password: wrong current passwords lock the account
//! like failed logins, so a stolen session cannot guess it (#1873).

#![cfg(all(feature = "sqlite", feature = "admin"))]

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

#[tokio::test]
async fn change_password_misses_lock_the_account() {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let pool: Pool = p.into();
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    let name = format!("cpw{}", std::process::id());
    AdminUser::new_with_password(&name, "correct-horse", true)
        .unwrap()
        .insert_pool(&pool)
        .await
        .unwrap();
    let app = Builder::new(pool)
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
