//! MySQL's default collation finds `alice` for `alicé` or `ａlice`, so
//! the admin login lock must follow the stored username, not only the
//! submitted one (#1609).
//!
//! Activated by `MYSQL_TEST_URL`; without it the test prints a skip and
//! passes. Drops and recreates the admin user tables in that database.

#![cfg(all(feature = "mysql", feature = "admin"))]

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use rustango::admin::{AdminUser, Builder};
use rustango::session::SessionSecret;
use rustango::sql::{sqlx, FetcherPool as _, Pool};
use tower::ServiceExt as _;

const CSRF: &str = "cccccccccccccccccccccccccccccccc";

async fn pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let p = sqlx::MySqlPool::connect(&url)
        .await
        .expect("MYSQL_TEST_URL");
    // Test-only DDL: reset the tables the admin creates on first use.
    for t in ["rustango_admin_users", "rustango_admin_totp"] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {t}"))
            .execute(&p)
            .await
            .expect("drop");
    }
    Some(p.into())
}

async fn login(app: &axum::Router, ip: &str, user: &str, pass: &str) -> StatusCode {
    let mut req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
        .body(Body::from(format!(
            "_csrf={CSRF}&username={}&password={pass}",
            urlencoding(user)
        )))
        .unwrap();
    let addr: SocketAddr = format!("{ip}:4000").parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    app.clone().oneshot(req).await.unwrap().status()
}

fn urlencoding(s: &str) -> String {
    s.bytes().map(|b| format!("%{b:02X}")).collect()
}

#[tokio::test]
async fn spellings_that_find_one_user_share_its_lock() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: MYSQL_TEST_URL not set");
        return;
    };
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(&pool)
        .await
        .unwrap();
    let mut u = AdminUser::new_with_password("lt_alice", "right-pass", false).unwrap();
    u.insert_pool(&pool).await.unwrap();

    let spellings = ["lt_alicé", "lt_ａlice", "LT_ALICÉ", "lt_álice", "lt_alíce"];
    // The premise: MySQL finds the row for every spelling.
    for s in spellings {
        let rows = AdminUser::objects()
            .filter("username", s)
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "MySQL must match `{s}` to lt_alice");
    }

    let app = Builder::new(pool)
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    // Five failures, each under a different spelling and address.
    for (n, s) in spellings.iter().enumerate() {
        let status = login(&app, &format!("10.71.0.{n}"), s, "wrong").await;
        assert_eq!(status, StatusCode::OK, "{s}");
    }
    let status = login(&app, "10.71.1.1", "lt_alice", "right-pass").await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "one lock per account"
    );
}
