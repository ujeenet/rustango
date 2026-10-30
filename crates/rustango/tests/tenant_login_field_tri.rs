//! `[auth] login_field = "email"`: tenant users sign in by email, any
//! case, on every backend, and the login lock follows the email.

#![cfg(all(
    feature = "tenancy",
    feature = "admin",
    feature = "sso",
    feature = "testkit"
))]

use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::{header, request::Parts, Request};
use rustango::core::Column as _;
use rustango::login_field::LoginField;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::auth_backends::{AuthBackend, ModelBackend};
use rustango::tenancy::{find_login_user, login_key, TenantSlug, User};
use rustango::tri_dialect_test;

/// The sign-in field, the throttle and the hash queue are process-global.
static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UNIQ: AtomicU64 = AtomicU64::new(0);

const PASS: &str = "right-pass-123";

fn unique(pool: &Pool, prefix: &str) -> String {
    format!(
        "{prefix}{}x{}x{}",
        pool.dialect().name(),
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn email_mode() {
    // Every test here wants email; an earlier one may have set it already.
    rustango::tenancy::configure_login_field(LoginField::Email);
    assert_eq!(rustango::tenancy::login_field(), LoginField::Email);
}

fn parts(user: &str, pass: &str) -> Parts {
    use base64::Engine as _;
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let addr: std::net::SocketAddr = format!("10.62.{}.{}:1", (n >> 8) & 255, n & 255)
        .parse()
        .unwrap();
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    Request::builder()
        .header(header::AUTHORIZATION, format!("Basic {creds}"))
        .extension(TenantSlug("login-field".into()))
        .extension(axum::extract::ConnectInfo(addr))
        .body(())
        .unwrap()
        .into_parts()
        .0
}

async fn seed(pool: &Pool, name: &str, email: Option<&str>) -> User {
    let mut u = User {
        username: name.to_owned(),
        password_hash: rustango::tenancy::password::hash(PASS).unwrap(),
        email: email.map(str::to_owned),
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed user");
    u
}

async fn signs_in(pool: &Pool, user: &str, pass: &str) -> Option<String> {
    match ModelBackend.authenticate(&parts(user, pass), pool).await {
        Ok(Some(u)) => Some(u.username),
        _ => None,
    }
}

/// The premise: `iexact` on the email column ignores case on this backend.
async fn iexact_matches_another_case(pool: &Pool) {
    let name = unique(pool, "probe");
    let email = format!("{name}@example.test");
    seed(pool, &name, Some(&email)).await;
    let rows = User::objects()
        .where_(User::email.iexact(email.to_uppercase()))
        .fetch(pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "iexact missed the row");
}

async fn email_signs_in_in_any_case(pool: &Pool) {
    let _g = SUITE.lock().await;
    email_mode();
    let name = unique(pool, "mail");
    let email = format!("{name}@Example.test");
    seed(pool, &name, Some(&email)).await;

    assert_eq!(signs_in(pool, &email, PASS).await.as_deref(), Some(&*name));
    let shouted = email.to_uppercase();
    assert_eq!(
        signs_in(pool, &shouted, PASS).await.as_deref(),
        Some(&*name),
        "a wrong-case email was refused"
    );
    assert_eq!(signs_in(pool, &email, "wrong-pass").await, None);
    // The username no longer signs in.
    assert_eq!(signs_in(pool, &name, PASS).await, None);
}

/// The lookup by username is unchanged, and never matches an email.
async fn the_username_field_matches_only_usernames(pool: &Pool) {
    let name = unique(pool, "user");
    let email = format!("{name}@example.test");
    seed(pool, &name, Some(&email)).await;

    let by_name = find_login_user(pool, LoginField::Username, &name)
        .await
        .unwrap();
    assert_eq!(by_name.map(|u| u.username), Some(name.clone()));
    let by_email = find_login_user(pool, LoginField::Username, &email)
        .await
        .unwrap();
    assert!(by_email.is_none(), "username mode matched an email");
    let email_by_name = find_login_user(pool, LoginField::Email, &name)
        .await
        .unwrap();
    assert!(email_by_name.is_none(), "email mode matched a username");
}

/// Failures under every spelling of one email lock it, and the lock key a
/// signed-in check uses is that same email.
async fn the_lock_follows_the_email(pool: &Pool) {
    let _g = SUITE.lock().await;
    email_mode();
    let name = unique(pool, "locked");
    let email = format!("{name}@example.test");
    let user = seed(pool, &name, Some(&email)).await;
    assert_eq!(login_key(LoginField::Email, &user), email);
    assert_eq!(login_key(LoginField::Username, &user), name);

    for i in 0..rustango::account_lockout::DEFAULT_MAX_ATTEMPTS {
        let spelled = if i % 2 == 0 {
            email.to_uppercase()
        } else {
            email.clone()
        };
        assert_eq!(signs_in(pool, &spelled, "wrong-pass").await, None);
    }
    assert_eq!(
        signs_in(pool, &format!(" {email} "), PASS).await,
        None,
        "the right password got past the lock"
    );
}

/// Users the email sign-in leaves out are counted for the boot warning.
/// Other suites share the table, so counts are compared as growth.
async fn gaps_count_missing_and_shared_emails(pool: &Pool) {
    let before = rustango::tenancy::email_gaps(pool).await.unwrap();
    let name = unique(pool, "gap");
    seed(pool, &name, None).await;
    // Email is unique, so a blank left by an earlier run blocks this one.
    let mut spaces = User {
        username: format!("{name}s"),
        email: Some("   ".to_owned()),
        ..rustango::testkit::user()
    };
    let blank_saved = usize::from(spaces.insert_pool(pool).await.is_ok());
    let shared = format!("{name}@example.test");
    seed(pool, &format!("{name}a"), Some(&shared)).await;
    // Spaces around it: the same address to sign-in, a new value to the index.
    seed(pool, &format!("{name}p"), Some(&format!("  {shared} "))).await;
    // MySQL's unique index ignores case, so the second one cannot exist there.
    let mut twin = User {
        username: format!("{name}b"),
        email: Some(shared.to_uppercase()),
        ..rustango::testkit::user()
    };
    let twin_saved = twin.insert_pool(pool).await.is_ok();

    let gaps = rustango::tenancy::email_gaps(pool).await.unwrap();
    assert!(
        gaps.without_email >= before.without_email + 1 + blank_saved,
        "{before:?} -> {gaps:?}"
    );
    let sharing = if twin_saved { 3 } else { 2 };
    assert!(
        gaps.shared_email >= before.shared_email + sharing,
        "{before:?} -> {gaps:?}"
    );
    if twin_saved {
        // A shared email signs nobody in.
        let found = find_login_user(pool, LoginField::Email, &shared)
            .await
            .unwrap();
        assert!(found.is_none(), "a shared email picked one user");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        iexact_matches_another_case,
        email_signs_in_in_any_case,
        the_username_field_matches_only_usernames,
        the_lock_follows_the_email,
        gaps_count_missing_and_shared_emails,
    ],
}
