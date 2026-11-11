//! Public helpers an app's own sign-in needs: the Bearer token and a
//! unique violation, without matching driver errors.
#![cfg(all(feature = "sqlite", feature = "_axum"))]

use axum::http::{header, HeaderMap};
use rustango::sql::Pool;

#[test]
fn bearer_token_reads_the_header_like_the_built_in_logins() {
    let mut h = HeaderMap::new();
    assert_eq!(rustango::bearer::token(&h), None);
    h.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
    assert_eq!(rustango::bearer::token(&h), Some("abc"));
    h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
    assert_eq!(rustango::bearer::token(&h), None);
}

#[tokio::test]
async fn a_duplicate_is_a_unique_violation() {
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    let run = |sql: &'static str| rustango::sql::raw_execute_pool(&pool, sql, Vec::new());
    run("CREATE TABLE asu_user (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)")
        .await
        .unwrap();
    run("INSERT INTO asu_user (email) VALUES ('a@x.test')")
        .await
        .unwrap();
    let err = run("INSERT INTO asu_user (email) VALUES ('a@x.test')")
        .await
        .expect_err("duplicate email");
    assert!(err.is_unique_violation(), "{err}");
    let err = run("INSERT INTO asu_user (email) VALUES (NULL)")
        .await
        .expect_err("null email");
    assert!(!err.is_unique_violation(), "{err}");
}
