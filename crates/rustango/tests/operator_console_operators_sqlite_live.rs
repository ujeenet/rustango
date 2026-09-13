#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Managing operators from the operator console, end to end through the
//! real router.
//!
//! The page was a read-only table whose own copy told the reader to go
//! and run `create-operator` on the production host. These tests cover
//! the surface that replaced it, and in particular the two writes that
//! must never succeed: the ones that lock somebody — or everybody — out
//! of the console being used to make them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto, FetcherPool as _};
use rustango::tenancy::operator_console::{router, router_with_provisioning, SessionSecret};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::{Operator, TenantPools};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Booted {
    app: axum::Router,
    cookie: String,
    registry: rustango::sql::Pool,
    me: String,
    my_id: i64,
    /// Kept so a second router built in a test can verify the same
    /// session cookie. A fresh `from_env_or_random()` would reject it,
    /// and the test would be measuring the key rather than the routes.
    secret: SessionSecret,
    _tmp: tempfile::TempDir,
    _migrations: tempfile::TempDir,
}

async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool));
    let migrations = tempfile::tempdir().expect("migrations dir");

    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &url,
        migrations.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    let registry = pools.registry_pool();
    let username = unique("op");
    let password = "letmein-please";
    let mut op = Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash(password).unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
    };
    op.insert_pool(&registry).await.expect("seed operator");
    let my_id = op.id.get().copied().unwrap_or_default();

    let provisioner = Provisioner::new(pools.clone(), url.clone(), migrations.path()).erased();
    let secret = SessionSecret::from_env_or_random();
    let app =
        router_with_provisioning(registry.clone(), pools.clone(), provisioner, secret.clone());

    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={username}&password={password}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    Booted {
        app,
        cookie,
        registry,
        me: username,
        my_id,
        secret,
        _tmp: tmp,
        _migrations: migrations,
    }
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Booted {
    async fn get(&self, uri: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post(&self, uri: &str, body: &str) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn operators(&self) -> Vec<Operator> {
        Operator::objects().fetch(&self.registry).await.unwrap()
    }

    async fn find(&self, username: &str) -> Option<Operator> {
        self.operators()
            .await
            .into_iter()
            .find(|o| o.username == username)
    }
}

/// The error path re-renders rather than redirecting, so the reason is
/// in the body either way.
///
/// Matched on the rendered `<div>`, not the bare class name: the page
/// inlines a stylesheet that *defines* `.alert-error`, so searching for
/// the class alone found the CSS on every page and reported a failure
/// as the text of a style rule.
async fn outcome(resp: axum::response::Response) -> String {
    if let Some(loc) = resp.headers().get("location") {
        return format!("redirect:{}", loc.to_str().unwrap());
    }
    let html = body_of(resp).await;
    const MARKER: &str = r#"<div class="alert alert-error">"#;
    match html.find(MARKER) {
        Some(i) => {
            let from = i + MARKER.len();
            let to = html[from..]
                .find("</div>")
                .map_or(html.len(), |end| from + end);
            format!("error:{}", html[from..to].trim())
        }
        None => "rendered (no error)".to_owned(),
    }
}

#[tokio::test]
async fn an_operator_created_through_the_form_can_sign_in() {
    let b = boot().await;
    let name = unique("newbie");
    let resp = b
        .post(
            "/operators",
            &format!("username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
        )
        .await;
    assert!(
        resp.status().is_redirection(),
        "a typed password should Post/Redirect/Get: {}",
        resp.status()
    );

    let created = b.find(&name).await.expect("the operator should exist");
    assert!(created.active, "new operators start active");

    // The real acceptance test: the credential works.
    let login = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={name}&password=hunter2hunter2"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let to = login
        .headers()
        .get("location")
        .map(|l| l.to_str().unwrap().to_owned())
        .unwrap_or_default();
    assert!(
        !to.contains("error"),
        "the new operator should be able to log in, got {to}"
    );
}

/// A generated password is a secret, so it is rendered on the POST
/// response and never put in a redirect URL — where it would reach the
/// history, the referrer and every access log in between.
#[tokio::test]
async fn a_generated_password_is_shown_once_and_never_in_a_url() {
    let b = boot().await;
    let name = unique("gen");
    let resp = b
        .post("/operators", &format!("username={name}&generate=on"))
        .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a generated password must be rendered, not redirected to"
    );
    assert!(
        resp.headers()
            .get("cache-control")
            .is_some_and(|v| v.to_str().unwrap().contains("no-store")),
        "a page holding a plaintext password must not be cached"
    );
    let html = body_of(resp).await;
    assert!(
        html.contains("alert-secret"),
        "the password should be shown once: {html}"
    );
    assert!(b.find(&name).await.is_some(), "and the operator created");
}

#[tokio::test]
async fn you_cannot_deactivate_yourself() {
    let b = boot().await;
    let resp = b.post(&format!("/operators/{}/active", b.my_id), "").await;
    let out = outcome(resp).await;
    assert!(out.starts_with("error:"), "should be refused, got {out}");
    assert!(out.contains("yourself"), "should say why: {out}");

    let me = b.find(&b.me).await.expect("still there");
    assert!(me.active, "and must still be active");
}

/// The other lock: with one operator left, deactivating them would
/// leave a console nobody can sign in to and no console path back.
///
/// Reached here by asking a *second* operator's row to be deactivated
/// while it is the only active one — the session owner cannot be the
/// target, because that is the self-check above.
#[tokio::test]
async fn deactivating_an_already_inactive_operator_says_so_rather_than_crying_lockout() {
    let b = boot().await;
    let name = unique("parked");
    b.post(
        "/operators",
        &format!("username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
    )
    .await;
    let id = b.find(&name).await.unwrap().id.get().copied().unwrap();

    // First deactivation: fine, the session owner stays active.
    let out = outcome(b.post(&format!("/operators/{id}/active"), "").await).await;
    assert!(out.starts_with("redirect:"), "should succeed: {out}");
    assert!(!b.find(&name).await.unwrap().active);

    // Second: a no-op. It must not be reported as a lockout — the
    // lockout check used to run first and counted the target itself,
    // so it answered "this is the last active operator" about an
    // operator who was not active at all.
    let resp = b.post(&format!("/operators/{id}/active"), "").await;
    let loc = resp
        .headers()
        .get("location")
        .expect("a no-op still redirects")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        loc.contains("already"),
        "should report a no-op, not a lockout: {loc}"
    );
    assert!(
        !loc.contains("last%20active") && !loc.contains("lock"),
        "should not claim a lockout: {loc}"
    );
}

#[tokio::test]
async fn deactivating_takes_effect_on_the_next_request() {
    let b = boot().await;
    let name = unique("bye");
    b.post(
        "/operators",
        &format!("username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
    )
    .await;

    // Sign the new operator in.
    let login = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={name}&password=hunter2hunter2"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let their_cookie = login
        .headers()
        .get("set-cookie")
        .expect("session")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let still_in = |cookie: String| {
        let app = b.app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .uri("/orgs")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };
    assert_eq!(
        still_in(their_cookie.clone()).await,
        StatusCode::OK,
        "should be signed in to start with"
    );

    let id = b.find(&name).await.unwrap().id.get().copied().unwrap();
    b.post(&format!("/operators/{id}/active"), "").await;

    assert!(
        still_in(their_cookie).await.is_redirection(),
        "the session must stop working immediately, not at expiry"
    );
}

/// Wait until the wall clock crosses into the next second.
///
/// Session `iat` and `password_changed_at` are both second-granularity,
/// and `require_session` rejects on `iat < password_changed_at` —
/// strictly less than, deliberately: `change_password_submit` does not
/// re-mint the cookie, so `<=` would sign an operator out the instant
/// they changed their own password. The consequence is that a reset in
/// the *same second* as a login leaves that session valid, which made
/// this test pass or fail depending on where the second boundary fell.
async fn cross_a_second_boundary() {
    let start = chrono::Utc::now().timestamp();
    while chrono::Utc::now().timestamp() == start {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A reset rotates `password_changed_at`, and `require_session` rejects
/// sessions issued before it. The page promises this; the promise has
/// to be true.
#[tokio::test]
async fn resetting_a_password_signs_that_operator_out() {
    let b = boot().await;
    let name = unique("rotate");
    b.post(
        "/operators",
        &format!("username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
    )
    .await;
    let id = b.find(&name).await.unwrap().id.get().copied().unwrap();

    let login = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={name}&password=hunter2hunter2"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let their_cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    cross_a_second_boundary().await;
    b.post(
        &format!("/operators/{id}/reset-password"),
        "password=totally-new-one&confirm_password=totally-new-one",
    )
    .await;

    let after = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/orgs")
                .header("cookie", &their_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        after.status().is_redirection(),
        "the old session must be rejected after a reset"
    );
    assert!(
        b.find(&name).await.unwrap().password_changed_at.is_some(),
        "password_changed_at is what does it, so it must be set"
    );
}

#[tokio::test]
async fn the_form_refuses_what_it_should() {
    let b = boot().await;
    let taken = unique("taken");
    b.post(
        "/operators",
        &format!("username={taken}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
    )
    .await;

    let cases = [
        (
            format!("username={taken}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
            "already exists",
        ),
        (
            "username=%20%20&password=hunter2hunter2&confirm_password=hunter2hunter2".to_owned(),
            "username is required",
        ),
        (
            format!(
                "username={}&password=hunter2hunter2&confirm_password=hunter2hunter2",
                "x".repeat(65)
            ),
            "at most 64",
        ),
        (
            "username=shorty&password=abc&confirm_password=abc".to_owned(),
            "at least 8",
        ),
        (
            "username=nomatch&password=hunter2hunter2&confirm_password=different0".to_owned(),
            "did not match",
        ),
        ("username=nopass".to_owned(), "password is required"),
        (
            "username=both&password=hunter2hunter2&confirm_password=hunter2hunter2&generate=on"
                .to_owned(),
            "Choose one",
        ),
    ];
    for (body, expect) in cases {
        let out = outcome(b.post("/operators", &body).await).await;
        assert!(
            out.starts_with("error:") && out.contains(expect),
            "`{body}` should have been refused with `{expect}`, got {out}"
        );
    }
}

#[tokio::test]
async fn the_write_routes_require_a_session() {
    let b = boot().await;
    for (uri, body) in [
        (
            "/operators".to_owned(),
            "username=evil&password=hunter2hunter2&confirm_password=hunter2hunter2",
        ),
        (format!("/operators/{}/active", b.my_id), ""),
        (
            format!("/operators/{}/reset-password", b.my_id),
            "generate=on",
        ),
    ] {
        let resp = b
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_redirection() || resp.status() == StatusCode::UNAUTHORIZED,
            "POST {uri} was reachable without a session: {}",
            resp.status()
        );
    }

    assert!(
        b.find("evil").await.is_none(),
        "an unauthenticated post must not create an operator"
    );
    assert!(
        b.find(&b.me).await.unwrap().active,
        "nor deactivate anybody"
    );
}

/// A read-only console offers the list and nothing else — and says so,
/// rather than rendering buttons that 404.
#[tokio::test]
async fn a_read_only_console_lists_but_does_not_manage() {
    let b = boot().await;
    // The same secret, so the existing cookie is valid here and the
    // test measures which routes are mounted rather than which key
    // signed the session.
    let read_only = router(b.registry.clone(), b.secret.clone());

    let listing = read_only
        .clone()
        .oneshot(
            Request::builder()
                .uri("/operators")
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listing.status(), StatusCode::OK, "the list still works");
    let html = body_of(listing).await;
    assert!(
        !html.contains("Add an operator"),
        "a read-only console must not offer the form: {html}"
    );

    let write = read_only
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/operators")
                .header("cookie", &b.cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "username=nope&password=hunter2hunter2&confirm_password=hunter2hunter2",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        write.status() == StatusCode::NOT_FOUND || write.status() == StatusCode::METHOD_NOT_ALLOWED,
        "and must not accept the write: {}",
        write.status()
    );
}

#[tokio::test]
async fn the_page_never_renders_a_password_hash() {
    let b = boot().await;
    let html = body_of(b.get("/operators").await).await;
    assert!(
        !html.contains("$argon2") && !html.contains("password_hash"),
        "the hash must not reach the page: {html}"
    );
}
