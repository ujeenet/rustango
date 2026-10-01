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
        sessions_revoked_at: None,
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
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
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
                    .header("cookie", "rustango_csrf=t")
                    .header("x-csrf-token", "t")
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
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
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
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
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

/// A reset writes a new hash, and `require_session` rejects sessions
/// minted under the old one. The page promises this.
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
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
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
        "a reset still stamps password_changed_at"
    );
}

/// #1338: a reset stamped in the same second as the login still signs
/// that operator out, and they can log in again with the new password.
#[tokio::test]
async fn a_reset_in_the_login_second_still_signs_that_operator_out() {
    let b = boot().await;
    let name = unique("same-second");
    b.post(
        "/operators",
        &format!("username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2"),
    )
    .await;
    let id = b.find(&name).await.unwrap().id.get().copied().unwrap();

    let login = |password: &'static str| {
        let app = b.app.clone();
        let name = name.clone();
        async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .header("cookie", "rustango_csrf=t")
                        .header("x-csrf-token", "t")
                        .uri("/login")
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(format!("username={name}&password={password}")))
                        .unwrap(),
                )
                .await
                .unwrap();
            resp.headers()
                .get("set-cookie")
                .expect("login sets a session cookie")
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        }
    };
    let orgs_status = |cookie: String| {
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

    let their_cookie = login("hunter2hunter2").await;
    let issued_at =
        rustango::tenancy::session::decode(&b.secret, their_cookie.split_once('=').unwrap().1)
            .unwrap()
            .iat;

    b.post(
        &format!("/operators/{id}/reset-password"),
        "password=totally-new-one&confirm_password=totally-new-one",
    )
    .await;
    // Pin the reset into the second the session was issued in.
    let mut row = b.find(&name).await.unwrap();
    row.password_changed_at = chrono::DateTime::from_timestamp(issued_at, 999_000_000);
    row.save_pool(&b.registry).await.unwrap();

    assert!(
        orgs_status(their_cookie).await.is_redirection(),
        "a session from the reset's own second must be rejected"
    );
    let fresh = login("totally-new-one").await;
    assert_eq!(
        orgs_status(fresh).await,
        StatusCode::OK,
        "a login after the reset must work"
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
                    .header("cookie", "rustango_csrf=t")
                    .header("x-csrf-token", "t")
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
                .header("cookie", "rustango_csrf=t")
                .header("x-csrf-token", "t")
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

/// #1710 — a signed-in operator's browser posting from another page.
/// `extra` are the headers the forger controls.
async fn forged_create(b: &Booted, name: &str, extra: &[(&str, &str)]) -> StatusCode {
    let mut req = Request::builder()
        .method("POST")
        .uri("/operators")
        .header("cookie", &b.cookie)
        .header("content-type", "application/x-www-form-urlencoded");
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let body = format!("username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2");
    b.app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

/// #1710 — without the token, or from a foreign Origin, a console POST
/// that rides the session cookie is refused and changes nothing.
#[tokio::test]
async fn a_forged_console_post_is_refused() {
    let b = boot().await;
    let name = unique("forged");
    assert_eq!(forged_create(&b, &name, &[]).await, StatusCode::FORBIDDEN);
    // A planted cookie half from a sibling tenant host, same token in
    // the form, but the page it came from is not the console.
    let planted = [
        ("cookie", "rustango_csrf=t"),
        ("x-csrf-token", "t"),
        ("origin", "http://t01.example.com"),
        ("host", "localhost"),
    ];
    assert_eq!(
        forged_create(&b, &name, &planted).await,
        StatusCode::FORBIDDEN
    );
    assert!(b.find(&name).await.is_none(), "no operator may be created");

    // Login CSRF: correct credentials, no token — no session is minted.
    let login = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={}&password=letmein-please",
                    b.me
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::FORBIDDEN);
    assert!(
        !login
            .headers()
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().unwrap_or("").starts_with("rustango_op_session=")),
        "a forged login must not mint a session"
    );

    // Forced logout: refused, and the session still works.
    let logout = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header("cookie", &b.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::FORBIDDEN);
    assert_eq!(b.get("/operators").await.status(), StatusCode::OK);
}

/// #1710 — the browser path: the page seeds the cookie and renders the
/// same token in the form, and posting that form field back works.
#[tokio::test]
async fn the_rendered_form_token_is_accepted() {
    let b = boot().await;
    let page = b.get("/operators").await;
    let seeded: Vec<String> = page
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next()?.strip_prefix("rustango_csrf="))
        .map(str::to_owned)
        .collect();
    // One cookie: two differing ones leave the form's token to luck.
    assert_eq!(seeded.len(), 1, "exactly one CSRF cookie: {seeded:?}");
    let set_cookie = seeded[0].clone();
    let html = body_of(page).await;
    assert!(
        html.contains(&format!(r#"name="_csrf" value="{set_cookie}""#)),
        "every form carries the cookie's token"
    );
    let name = unique("real");
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/operators")
                .header("cookie", &b.cookie)
                .header("cookie", format!("rustango_csrf={set_cookie}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf={set_cookie}&username={name}&password=hunter2hunter2&confirm_password=hunter2hunter2"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_redirection(), "{}", resp.status());
    assert!(b.find(&name).await.is_some());
}

/// #1694 — logout reads the session cookie, so the signal names the operator.
#[tokio::test]
async fn logout_signal_names_the_signed_in_operator() {
    use rustango::signals::auth::{connect_user_logged_out, disconnect_user_logged_out};
    let b = boot().await;
    let seen: Arc<std::sync::Mutex<Vec<Option<i64>>>> = Arc::default();
    let sink = seen.clone();
    let id = connect_user_logged_out(move |ctx| {
        sink.lock().unwrap().push(ctx.user_id);
        async {}
    });
    let resp = b.post("/logout", "").await;
    disconnect_user_logged_out(id);
    assert!(resp.status().is_redirection(), "{}", resp.status());
    // Other tests may log out at the same time; look for this operator only.
    let seen = seen.lock().unwrap();
    assert!(seen.contains(&Some(b.my_id)), "{seen:?}");
}

/// #1710 — the browser login: `/login` seeds one cookie, its form and the
/// page's `<meta>` carry that token, and posting the form signs in.
#[tokio::test]
async fn the_login_form_round_trip_signs_in() {
    let b = boot().await;
    let page = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let seeded: Vec<String> = page
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next()?.strip_prefix("rustango_csrf="))
        .map(str::to_owned)
        .collect();
    assert_eq!(seeded.len(), 1, "{seeded:?}");
    let token = &seeded[0];
    let html = body_of(page).await;
    assert!(
        html.contains(&format!(r#"name="_csrf" value="{token}""#)),
        "login form token"
    );
    let login = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("cookie", format!("rustango_csrf={token}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf={token}&username={}&password=letmein-please",
                    b.me
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        login
            .headers()
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().unwrap_or("").starts_with("rustango_op_session=")),
        "the real form signs in: {}",
        login.status()
    );

    // Pages the layout wraps expose the same token to scripts.
    let page = b.get("/operators").await;
    let html = body_of(page).await;
    assert!(
        html.contains(r#"<meta name="csrf-token" content=""#) && !html.contains(r#"content="">"#),
        "the layout's csrf-token meta is filled"
    );

    // Cacheable assets carry no cookie a shared cache could store.
    let asset = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__static__/rustango.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(asset.headers().get("set-cookie").is_none());
}

/// #1710 — every POST form in a console template carries the token, or
/// sends it as a header from script (the multipart branding form).
#[test]
fn every_console_post_form_carries_the_token() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tenancy/templates");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("op_") {
            continue;
        }
        let html = strip_tera_comments(&std::fs::read_to_string(&path).unwrap());
        for form in html.split("<form").skip(1) {
            let form = form.split("</form>").next().unwrap_or(form);
            let head = form.split('>').next().unwrap_or("").to_ascii_lowercase();
            let head = head.replace('\'', "\"");
            if !head.contains(r#"method="post""#) {
                continue;
            }
            // The multipart branding form: its own submit handler must
            // send the header, not just any script in the file.
            let by_header = head.contains(r#"id="branding-form""#)
                && html
                    .split(r#"getElementById("branding-form")"#)
                    .nth(1)
                    .and_then(|h| h.split("</script>").next())
                    .is_some_and(|h| h.contains(r#""X-CSRF-Token": csrfToken()"#));
            if !form.contains("{{ csrf_input") && !by_header {
                missing.push(format!("{name}: <form{head}>"));
            }
        }
        // And every script POST sends the header.
        for call in html.split("fetch(").skip(1) {
            let call = call.split("});").next().unwrap_or(call);
            if call.contains(r#"method: "POST""#)
                && !call.contains(r#""X-CSRF-Token": csrfToken()"#)
            {
                missing.push(format!("{name}: fetch({}", &call[..call.len().min(60)]));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "console POSTs without a CSRF token:\n{}",
        missing.join("\n")
    );
}

fn strip_tera_comments(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("{#") {
        out.push_str(&rest[..i]);
        rest = rest[i..].find("#}").map_or("", |j| &rest[i + j + 2..]);
    }
    out.push_str(rest);
    out
}
