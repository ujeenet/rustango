//! Backing test for `docs/security.md` — CSRF on admin mutations (#1395).
//!
//! The page said the auto-admin "enables CSRF on every mutation by
//! default, and there is no way to opt out". Neither half was true: the
//! only protected route was `POST /login`, and `login.html` was the only
//! one of fourteen templates rendering a token. Create, update, delete,
//! bulk actions and audit cleanup all accepted a cross-site POST riding
//! the administrator's session cookie — audit cleanup included, so the
//! same request class could erase its own trace.
//!
//! The sentence is why it survived. An auditor reading "there is no way
//! to opt out" stops checking.
//!
//! These assert the two halves that had to land together: the layer that
//! rejects a tokenless mutation, and a token in the rendered form. Either
//! alone is useless — the layer without tokens turns every admin
//! mutation into a 403, and tokens without the layer protect nothing.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "csrf"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "csrf_post", admin(list_display = "title"))]
pub struct CsrfPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

async fn pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "csrf_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

/// The admin as a real deployment builds it: session auth on. That is
/// the configuration CSRF is for — cookie-borne credentials.
fn app_with_session_auth(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool)
        .admin_prefix("")
        .with_session_auth(rustango::session::SessionSecret::from_bytes(vec![7u8; 32]))
        .build()
}

/// A mutation with no token is refused, and refused as a *CSRF* failure
/// rather than an auth redirect.
///
/// The distinction matters: the layers are ordered so CSRF runs outside
/// the session gate, which is what makes this 403 instead of a 303 to
/// the login page. An unauthenticated attacker probing the admin should
/// not be able to tell the two apart, and more importantly the check
/// must not depend on the session being valid.
#[tokio::test]
async fn a_post_without_a_token_is_rejected() {
    let app = app_with_session_auth(pool().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/csrf_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=forged"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a tokenless admin mutation must be refused — this is the hole"
    );
}

/// The delete route specifically, because it is destructive and was
/// among the exposed set.
#[tokio::test]
async fn a_delete_without_a_token_is_rejected() {
    let app = app_with_session_auth(pool().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/csrf_post/1/delete")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A cookie that does not match the submitted token is refused too —
/// otherwise "has a token" would be the check rather than "has *the*
/// token", and an attacker can always put some value in a form.
#[tokio::test]
async fn a_mismatched_token_is_rejected() {
    let app = app_with_session_auth(pool().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/csrf_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, "rustango_csrf=aaaaaaaaaaaaaaaa")
                .body(Body::from("title=forged&_csrf=bbbbbbbbbbbbbbbb"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the submitted token must match the cookie, not merely exist"
    );
}

/// A matching pair passes the CSRF layer. It then hits the session gate
/// and redirects to login, which is the correct next failure — the point
/// is that it is no longer a 403, so the layer is not simply refusing
/// everything.
///
/// Without this, the three tests above would also pass if CSRF rejected
/// every request unconditionally, which would be a different bug.
#[tokio::test]
async fn a_matching_token_passes_the_csrf_layer() {
    let app = app_with_session_auth(pool().await);
    let token = "cccccccccccccccccccccccccccccccc";
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/csrf_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={token}"))
                .body(Body::from(format!("title=ok&_csrf={token}")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a valid token must get past CSRF — otherwise the fix is `deny everything`"
    );
}

/// A GET seeds the cookie even though it is redirected to login, so the
/// browser has something to match when the form is finally submitted.
///
/// This is why the token middleware sits outside the session gate: if it
/// ran inside, an unauthenticated first visit would set no cookie and the
/// first post-login submission would fail.
#[tokio::test]
async fn a_get_seeds_the_csrf_cookie() {
    let app = app_with_session_auth(pool().await);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/csrf_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let cookies: Vec<String> = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect();

    assert!(
        cookies.iter().any(|c| c.starts_with("rustango_csrf=")),
        "a GET must seed the CSRF cookie; got {cookies:?}"
    );
}

/// Every POST form in every admin template renders a token.
///
/// Source-level on purpose. The regression this guards is not "the
/// current forms lost their tokens" — it is *a new template arriving
/// without one*, which no end-to-end test covers because the test would
/// have to be written alongside the template that forgot.
///
/// It reads the templates out of the source tree rather than the
/// compiled binary, so a form added tomorrow is checked without anyone
/// remembering to extend this file. The layer would turn such a form
/// into a 403 on submit, which is safe but looks like a broken admin.
#[test]
fn every_admin_post_form_renders_a_csrf_token() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/admin/templates");
    let mut checked = 0usize;
    let mut missing: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(&dir)
        .expect("read admin templates")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("html") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();

        // Each `<form … method="post"` opening, and the slice up to the
        // matching `</form>` — enough to see whether a token is inside
        // that form rather than merely somewhere on the page.
        let mut rest = text.as_str();
        while let Some(i) = rest.find("<form") {
            let after = &rest[i..];
            let end = after.find("</form>").map_or(after.len(), |e| e + 7);
            let block = &after[..end];
            if block.contains("method=\"post\"") || block.contains("method='post'") {
                checked += 1;
                if !block.contains("csrf_input") && !block.contains("_csrf") {
                    missing.push(format!(
                        "{name}: {}",
                        block.lines().next().unwrap_or("").trim()
                    ));
                }
            }
            rest = &after[end.max(1)..];
        }
    }

    assert!(
        checked > 0,
        "found no POST forms in the admin templates — this guard stopped reading \
         what it claims to"
    );
    assert!(
        missing.is_empty(),
        "{} admin POST form(s) render no CSRF token, so submitting them 403s:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
}
