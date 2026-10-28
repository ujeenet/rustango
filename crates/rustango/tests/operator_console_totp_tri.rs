//! The operator console's TOTP second factor, on every backend.

#![cfg(all(feature = "tenancy", feature = "totp", feature = "testkit"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool, UpdaterPool as _};
use rustango::tenancy::{Operator, OperatorTotp, TenantPoolInvalidator, TenantPools};
use rustango::totp::{self, TotpSecret};
use rustango::tri_dialect_test;
use tower::ServiceExt;

const PASSWORD: &str = "operator-password-totp";
const SESSION: &str = "rustango_op_session";
const PENDING: &str = "rustango_op_totp";
const KEY: &str = "operator-console-totp-tri-key-0123456789";

async fn setup(pool: &Pool) {
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
    // Operator ids restart when another suite recreates the operators
    // table; the console must create this one itself.
    rustango::testkit::matrix::drop_table(pool, "rustango_operator_totp").await;
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

fn invalidator(pool: &Pool) -> Arc<dyn TenantPoolInvalidator> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => Arc::new(TenantPools::<sqlx::Postgres>::new(p.clone())),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => Arc::new(TenantPools::<sqlx::MySql>::new(p.clone())),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => Arc::new(TenantPools::<sqlx::Sqlite>::new(p.clone())),
        #[allow(unreachable_patterns)]
        _ => unreachable!("unknown backend"),
    }
}

/// A console router; `require` sets the policy, which is read at build.
fn app(pool: &Pool, require: bool) -> Router {
    if require {
        std::env::set_var("RUSTANGO_OPERATOR_REQUIRE_TOTP", "1");
    }
    let app = rustango::tenancy::operator_console::router_with_pools(
        pool.clone(),
        invalidator(pool),
        rustango::tenancy::operator_console::SessionSecret::from_bytes(vec![6u8; 32]),
    );
    std::env::remove_var("RUSTANGO_OPERATOR_REQUIRE_TOTP");
    app
}

async fn operator(pool: &Pool) -> Operator {
    let mut op = Operator {
        id: Auto::default(),
        username: unique("totp"),
        password_hash: rustango::tenancy::password::hash_async(PASSWORD)
            .await
            .unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(pool).await.expect("seed operator");
    op
}

async fn send(app: &Router, method: &str, uri: &str, cookie: &str, body: String) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, "console.test")
                .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                .header("x-csrf-token", "t")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn html(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// `name=value` of the `name` cookie the response sets, if any.
fn set_cookie(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|v| v.starts_with(&format!("{name}=")) && v.len() > name.len() + 1)
        .map(str::to_owned)
}

fn location(resp: &Response) -> String {
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn password_step(app: &Router, op: &Operator) -> Response {
    send(
        app,
        "POST",
        "/login",
        "",
        format!("username={}&password={PASSWORD}&next=/orgs", op.username),
    )
    .await
}

/// The setup key the page shows, read back as a secret.
fn shown_key(page: &str) -> TotpSecret {
    let at = page.find("class=\"key\">").expect("setup key shown") + "class=\"key\">".len();
    let key: String = page[at..].chars().take_while(|c| *c != '<').collect();
    TotpSecret::from_base32(key.trim()).expect("base32 key")
}

/// Unix seconds early in a 30 s step, so the codes below stay in the window.
async fn fresh_step() -> u64 {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    if now() % 30 > 15 {
        tokio::time::sleep(std::time::Duration::from_secs(31 - now() % 30)).await;
    }
    now()
}

fn wrong_code(secret: &TotpSecret, now: u64) -> String {
    let valid: Vec<String> = [now - 30, now, now + 30]
        .iter()
        .map(|t| totp::generate_at(secret, *t, 30, 6))
        .collect();
    (0..1_000_000u32)
        .map(|n| format!("{n:06}"))
        .find(|c| !valid.contains(c))
        .unwrap()
}

async fn audit_count(pool: &Pool, op: &Operator, verb: &str) -> usize {
    let id = op.id.get().copied().unwrap();
    rustango::audit::AuditLog::objects()
        .filter("source", format!("operator:{id}:{verb}"))
        .fetch(pool)
        .await
        .expect("audit rows")
        .len()
}

/// Sign in with the password alone; the session cookie.
async fn password_session(app: &Router, op: &Operator) -> String {
    let resp = password_step(app, op).await;
    assert_eq!(location(&resp), "/orgs", "no device: the password signs in");
    set_cookie(&resp, SESSION).expect("session cookie")
}

/// Enroll from the account page; the secret.
async fn enroll(pool: &Pool, app: &Router, op: &Operator, session: &str, now: u64) -> TotpSecret {
    let page = html(send(app, "GET", "/account/totp", session, String::new()).await).await;
    let secret = shown_key(&page);
    let confirm = totp::generate_at(&secret, now - 30, 30, 6);
    let resp = send(
        app,
        "POST",
        "/account/totp",
        session,
        format!("totp_code={confirm}"),
    )
    .await;
    assert!(
        html(resp).await.contains("now on"),
        "the confirming code turns it on"
    );
    assert_eq!(audit_count(pool, op, "totp_enroll").await, 1);
    // Stored sealed, never as the key itself.
    let stored: Vec<String> = OperatorTotp::objects()
        .filter("operator_id", op.id.get().copied().unwrap())
        .values_list_flat("secret")
        .fetch(pool)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].starts_with("enc:v1:"), "{}", stored[0]);
    assert!(!stored[0].contains(&secret.to_base32()));
    secret
}

/// Enroll, sign in with a code, refuse a wrong and a replayed one, disable.
async fn enroll_sign_in_replay_disable(pool: &Pool) {
    let app = app(pool, false);
    let op = operator(pool).await;
    let now = fresh_step().await;
    let session = password_session(&app, &op).await;
    let secret = enroll(pool, &app, &op, &session, now).await;

    // The password now sets only the pending cookie.
    let resp = password_step(&app, &op).await;
    assert!(
        location(&resp).starts_with("/login/totp"),
        "{}",
        location(&resp)
    );
    assert!(
        set_cookie(&resp, SESSION).is_none(),
        "no session before the code"
    );
    let pending = set_cookie(&resp, PENDING).expect("pending cookie");
    let form = html(send(&app, "GET", "/login/totp", &pending, String::new()).await).await;
    assert!(form.contains("name=\"totp_code\""), "{form}");
    assert!(
        !form.contains("class=\"key\""),
        "an enrolled sign-in shows no key"
    );

    let wrong = wrong_code(&secret, now);
    let resp = send(
        &app,
        "POST",
        "/login/totp",
        &pending,
        format!("totp_code={wrong}&next=/orgs"),
    )
    .await;
    assert!(
        set_cookie(&resp, SESSION).is_none(),
        "a wrong code signs in"
    );
    assert!(location(&resp).starts_with("/login/totp"));
    assert_eq!(audit_count(pool, &op, "totp_failed").await, 1);
    let page = html(send(&app, "GET", &location(&resp), &pending, String::new()).await).await;
    assert!(
        page.contains("not accepted"),
        "a wrong code says so: {page}"
    );

    let code = totp::generate_at(&secret, now, 30, 6);
    let resp = send(
        &app,
        "POST",
        "/login/totp",
        &pending,
        format!("totp_code={code}&next=/orgs"),
    )
    .await;
    assert_eq!(location(&resp), "/orgs");
    let session = set_cookie(&resp, SESSION).expect("the code signs in");
    let orgs = send(&app, "GET", "/orgs", &session, String::new()).await;
    assert_eq!(orgs.status(), StatusCode::OK);

    // The same code again, on a fresh password step.
    let again = set_cookie(&password_step(&app, &op).await, PENDING).unwrap();
    let resp = send(
        &app,
        "POST",
        "/login/totp",
        &again,
        format!("totp_code={code}&next=/orgs"),
    )
    .await;
    assert!(
        set_cookie(&resp, SESSION).is_none(),
        "a replayed code signs in"
    );
    let page = html(send(&app, "GET", &location(&resp), &again, String::new()).await).await;
    assert!(
        page.contains("not accepted"),
        "a replayed code says so: {page}"
    );

    // Off with a later code; then the password alone signs in again.
    let later = totp::generate_at(&secret, now + 30, 30, 6);
    let resp = send(
        &app,
        "POST",
        "/account/totp/disable",
        &session,
        format!("totp_code={code}"),
    )
    .await;
    assert!(
        html(resp).await.contains("to turn it off"),
        "a used code was not refused"
    );
    let resp = send(
        &app,
        "POST",
        "/account/totp/disable",
        &session,
        format!("totp_code={later}"),
    )
    .await;
    assert!(html(resp).await.contains("is off"));
    assert_eq!(audit_count(pool, &op, "totp_disable").await, 1);
    password_session(&app, &op).await;
}

/// Neither the pending cookie nor its value as a session opens a page.
async fn the_pending_state_opens_no_page(pool: &Pool) {
    let app = app(pool, false);
    let op = operator(pool).await;
    let now = fresh_step().await;
    let session = password_session(&app, &op).await;
    enroll(pool, &app, &op, &session, now).await;
    let pending = set_cookie(&password_step(&app, &op).await, PENDING).unwrap();
    let value = pending.split_once('=').unwrap().1.to_owned();
    let as_session = format!("{SESSION}={value}");

    for cookie in [pending.as_str(), as_session.as_str()] {
        for (method, path) in [
            ("GET", "/"),
            ("GET", "/operators"),
            ("GET", "/orgs"),
            ("GET", "/audit"),
            ("GET", "/change-password"),
            ("GET", "/account/totp"),
            ("GET", "/orgs/acme/edit"),
            ("GET", "/orgs/acme/hosts"),
            ("POST", "/operators"),
            ("POST", "/account/totp/disable"),
        ] {
            let resp = send(&app, method, path, cookie, "username=intruder".into()).await;
            assert!(
                location(&resp).starts_with("/login?"),
                "{method} {path} with `{cookie}`: {} {}",
                resp.status(),
                location(&resp)
            );
        }
    }
    let intruders = Operator::objects()
        .filter("username", "intruder")
        .fetch(pool)
        .await
        .unwrap();
    assert!(
        intruders.is_empty(),
        "the pending state created an operator"
    );
    // The code step itself needs the pending cookie.
    let resp = send(&app, "GET", "/login/totp", "", String::new()).await;
    assert!(location(&resp).starts_with("/login?"));
}

/// Under the required policy, an operator without a device enrolls at sign-in.
async fn a_required_policy_enrolls_at_sign_in(pool: &Pool) {
    let app = app(pool, true);
    let op = operator(pool).await;
    let now = fresh_step().await;
    let resp = password_step(&app, &op).await;
    assert!(
        set_cookie(&resp, SESSION).is_none(),
        "required: no password-only session"
    );
    let pending = set_cookie(&resp, PENDING).expect("pending cookie");
    let page = html(send(&app, "GET", "/login/totp", &pending, String::new()).await).await;
    let secret = shown_key(&page);
    let code = totp::generate_at(&secret, now, 30, 6);
    let resp = send(
        &app,
        "POST",
        "/login/totp",
        &pending,
        format!("totp_code={code}&next=/orgs"),
    )
    .await;
    assert_eq!(location(&resp), "/orgs");
    assert!(set_cookie(&resp, SESSION).is_some());
    assert_eq!(audit_count(pool, &op, "totp_enroll").await, 1);
    let confirmed: Vec<bool> = OperatorTotp::objects()
        .filter("operator_id", op.id.get().copied().unwrap())
        .values_list_flat("confirmed")
        .fetch(pool)
        .await
        .unwrap();
    assert_eq!(confirmed, vec![true]);
}

/// A weak hash is upgraded once the code passes, never by the password alone.
async fn a_code_sign_in_upgrades_a_weak_hash(pool: &Pool) {
    use rustango::passwords::needs_rehash;
    // argon2id m=8,t=1,p=1 of "pw-weak-hash".
    const WEAK: &str = "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0c2FsdA$Tk2Yd9kayx5c9Zj6ox/S6WGhYHdSHI6LCO1srlux3Ns";
    let app = app(pool, true);
    let op = operator(pool).await;
    let id = op.id.get().copied().unwrap();
    Operator::objects()
        .filter("id", id)
        .update()
        .set("password_hash", WEAK)
        .execute_pool(pool)
        .await
        .unwrap();
    let stored = || async {
        Operator::objects()
            .filter("id", id)
            .fetch(pool)
            .await
            .unwrap()
            .remove(0)
            .password_hash
    };
    let now = fresh_step().await;
    let body = format!("username={}&password=pw-weak-hash&next=/orgs", op.username);
    let resp = send(&app, "POST", "/login", "", body).await;
    let pending = set_cookie(&resp, PENDING).expect("pending cookie");
    assert_eq!(stored().await, WEAK, "the password alone wrote the hash");
    let page = html(send(&app, "GET", "/login/totp", &pending, String::new()).await).await;
    let code = totp::generate_at(&shown_key(&page), now, 30, 6);
    let resp = send(
        &app,
        "POST",
        "/login/totp",
        &pending,
        format!("totp_code={code}&next=/orgs"),
    )
    .await;
    let session = set_cookie(&resp, SESSION).expect("the code signs in");
    let new = stored().await;
    assert_ne!(new, WEAK, "the weak hash is still stored");
    assert!(!needs_rehash(&new), "{new}");
    let orgs = send(&app, "GET", "/orgs", &session, String::new()).await;
    assert_eq!(
        orgs.status(),
        StatusCode::OK,
        "the session fits the new hash"
    );
}

/// Without a key the page says how to set one, not a link back to itself.
async fn without_a_key_the_page_says_how_to_set_it(pool: &Pool) {
    let app = app(pool, false);
    let op = operator(pool).await;
    let session = password_session(&app, &op).await;
    // Off dev/test, so the saved dev key does not stand in.
    std::env::set_var("RUSTANGO_ENV", "staging");
    std::env::remove_var("RUSTANGO_SECRET_KEY");
    let page = html(send(&app, "GET", "/account/totp", &session, String::new()).await).await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    std::env::remove_var("RUSTANGO_ENV");
    assert!(page.contains("openssl rand -base64 48"), "{page}");
    assert!(
        !page.contains("Set it up"),
        "a link back to this page: {page}"
    );
}

/// Another operator removes a lost device from `/operators`, audited;
/// your own device is refused there.
async fn another_operator_resets_a_lost_device(pool: &Pool) {
    let app = app(pool, false);
    let lost = operator(pool).await;
    let now = fresh_step().await;
    let session = password_session(&app, &lost).await;
    enroll(pool, &app, &lost, &session, now).await;
    let admin = operator(pool).await;
    let admin_session = password_session(&app, &admin).await;
    let lost_id = lost.id.get().copied().unwrap();
    let devices = || async {
        OperatorTotp::objects()
            .filter("operator_id", lost_id)
            .fetch(pool)
            .await
            .unwrap()
            .len()
    };

    let own = send(
        &app,
        "POST",
        &format!("/operators/{lost_id}/reset-totp"),
        &session,
        String::new(),
    )
    .await;
    assert!(
        html(own).await.contains("/account/totp"),
        "own device refused"
    );
    assert_eq!(devices().await, 1);

    let resp = send(
        &app,
        "POST",
        &format!("/operators/{lost_id}/reset-totp"),
        &admin_session,
        String::new(),
    )
    .await;
    assert!(
        location(&resp).starts_with("/operators?notice=removed"),
        "{}",
        location(&resp)
    );
    assert_eq!(devices().await, 0);
    assert_eq!(audit_count(pool, &admin, "operator_totp_reset").await, 1);
    password_session(&app, &lost).await;
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        without_a_key_the_page_says_how_to_set_it,
        enroll_sign_in_replay_disable,
        the_pending_state_opens_no_page,
        a_required_policy_enrolls_at_sign_in,
        a_code_sign_in_upgrades_a_weak_hash,
        another_operator_resets_a_lost_device,
    ]
);
