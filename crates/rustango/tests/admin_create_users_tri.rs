//! Tenant users and SSO providers created and edited through the admin,
//! on every backend (#1763, #1764).

#![cfg(all(feature = "admin", feature = "tenancy", feature = "admin-sso"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::casts::{CastValue as _, EncryptedString};
use rustango::core::{Column as _, Filter, Op, SelectQuery, WhereExpr};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::auth::authenticate_user_pool;
use rustango::tenancy::User;
use rustango::tri_dialect_test;
use tower::ServiceExt;

async fn setup(pool: &Pool) {
    std::env::set_var("RUSTANGO_SECRET_KEY", "admin-create-users-tri-key");
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

/// Unique per run: the shared tables keep earlier runs' rows.
fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

async fn send(pool: &Pool, method: Method, uri: &str, body: String) -> (StatusCode, String) {
    send_as(pool, None, method, uri, body).await
}

/// `perms: Some(..)` is a non-superuser holding those codenames.
async fn send_as(
    pool: &Pool,
    perms: Option<&[&str]>,
    method: Method,
    uri: &str,
    body: String,
) -> (StatusCode, String) {
    // One database stands in for both; the shared SSO providers live on the registry.
    let mut builder = rustango::admin::Builder::new(pool.clone()).admin_prefix("");
    if let Some(perms) = perms {
        builder = builder.with_user_perms(perms.iter().map(|p| (*p).to_owned()));
    }
    let app = if uri.starts_with("/rustango_shared_sso_providers") {
        builder.registry_mode()
    } else {
        builder
    }
    .build();
    let resp = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Test values are URL-safe, so no encoding is needed.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The `<input …>` tag for `name`.
fn input_tag<'a>(html: &'a str, name: &str) -> &'a str {
    let at = html
        .find(&format!(r#"name="{name}""#))
        .unwrap_or_else(|| panic!("no input {name}: {html}"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    &html[start..=end]
}

async fn user_named(pool: &Pool, name: &str) -> User {
    User::objects()
        .where_(User::username.eq(name.to_owned()))
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no user {name}"))
}

async fn create_user(pool: &Pool, name: &str, password: &str) -> (StatusCode, String) {
    let body = form(&[
        ("username", name),
        ("password_hash", password),
        ("active", "true"),
        ("data", ""),
    ]);
    send(pool, Method::POST, "/rustango_users", body).await
}

async fn logs_in(pool: &Pool, name: &str, password: &str) -> bool {
    authenticate_user_pool(pool, name, password)
        .await
        .unwrap()
        .is_some()
}

async fn new_user_form_does_not_block_submit(pool: &Pool) {
    let (status, html) = send(pool, Method::GET, "/rustango_users/new", String::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let created = input_tag(&html, "created_at");
    assert!(created.contains("readonly"), "{created}");
    assert!(!created.contains("required"), "{created}");
    let password = input_tag(&html, "password_hash");
    assert!(password.contains(r#"type="password""#), "{password}");
    assert!(password.contains("required"), "{password}");
}

async fn admin_created_user_can_log_in(pool: &Pool) {
    let name = unique("adm");
    let (status, body) = create_user(pool, &name, "pw-1763-first").await;
    assert!(status.is_redirection(), "{status}: {body}");
    let user = user_named(pool, &name).await;
    assert!(
        user.password_hash.starts_with("$argon2"),
        "stored in the clear"
    );
    assert!(logs_in(pool, &name, "pw-1763-first").await);
    // The detail page never shows the hash.
    let pk = user.id.get().copied().unwrap();
    let (_, detail) = send(
        pool,
        Method::GET,
        &format!("/rustango_users/{pk}"),
        String::new(),
    )
    .await;
    assert!(!detail.contains(&user.password_hash), "{detail}");
}

/// A login over a hash weaker than today's cost stores a fresh one, but
/// only once it completes: a password alone writes nothing (#2093).
async fn a_login_upgrades_a_weak_hash(pool: &Pool) {
    use rustango::passwords::needs_rehash;
    use rustango::sql::UpdaterPool as _;
    // argon2id m=8,t=1,p=1 of "pw-weak-hash".
    const WEAK: &str = "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0c2FsdA$Tk2Yd9kayx5c9Zj6ox/S6WGhYHdSHI6LCO1srlux3Ns";
    let name = unique("weak");
    let (status, body) = create_user(pool, &name, "pw-weak-hash").await;
    assert!(status.is_redirection(), "{status}: {body}");
    User::objects()
        .where_(User::username.eq(name.clone()))
        .update()
        .set("password_hash", WEAK)
        .execute_pool(pool)
        .await
        .unwrap();
    let verified = authenticate_user_pool(pool, &name, "pw-weak-hash")
        .await
        .unwrap()
        .expect("right password");
    assert_eq!(
        user_named(pool, &name).await.password_hash,
        WEAK,
        "the password alone stored a new hash"
    );
    let user = verified.complete(pool).await;
    let new = user_named(pool, &name).await.password_hash;
    assert_eq!(user.password_hash, new);
    assert_ne!(new, WEAK, "the weak hash is still stored");
    assert!(!needs_rehash(&new), "{new}");
    assert!(logs_in(pool, &name, "pw-weak-hash").await);
}

async fn editing_a_user_keeps_or_rotates_the_password(pool: &Pool) {
    let name = unique("edt");
    create_user(pool, &name, "pw-1763-old").await;
    let pk = user_named(pool, &name).await.id.get().copied().unwrap();
    let uri = format!("/rustango_users/{pk}");

    // An empty password keeps the stored one.
    let edit = |pw: &'static str| {
        form(&[
            ("username", name.as_str()),
            ("password_hash", pw),
            ("active", "true"),
            ("data", ""),
        ])
    };
    let (status, body) = send(pool, Method::POST, &uri, edit("")).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(logs_in(pool, &name, "pw-1763-old").await);
    assert!(user_named(pool, &name).await.password_changed_at.is_none());

    // A new one replaces it and ends older sessions.
    let (status, body) = send(pool, Method::POST, &uri, edit("pw-1763-new")).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(logs_in(pool, &name, "pw-1763-new").await);
    assert!(!logs_in(pool, &name, "pw-1763-old").await);
    assert!(user_named(pool, &name).await.password_changed_at.is_some());

    // The audit log records the change, never the hash.
    let log = rustango::audit::fetch_for_entity_pool(pool, "rustango_users", &pk.to_string())
        .await
        .unwrap();
    let text: Vec<String> = log.iter().map(|e| e.changes.to_string()).collect();
    assert!(text.iter().all(|t| !t.contains("$argon2")), "{text:?}");
    assert!(
        log.iter().any(|e| {
            e.changes["password_hash"]["after"] == "[changed]"
                && !e.changes["password_changed_at"].is_null()
        }),
        "{text:?}"
    );
}

async fn admin_created_provider_secret_is_encrypted(pool: &Pool) {
    let slug = unique("p");
    let body = form(&[
        ("slug", slug.as_str()),
        ("label", "Corp"),
        ("kind", "oidc"),
        ("client_id", "cid"),
        ("client_secret", "plain-secret-1764"),
        ("enabled", "true"),
        ("sort_order", "0"),
    ]);
    let (status, body) = send(pool, Method::POST, "/rustango_shared_sso_providers", body).await;
    assert!(status.is_redirection(), "{status}: {body}");

    let schema = rustango::migrate::registered_models()
        .into_iter()
        .find(|m| m.table == "rustango_shared_sso_providers")
        .unwrap();
    let fields: Vec<_> = schema.scalar_fields().collect();
    let query = SelectQuery::new(schema).where_clause(WhereExpr::Predicate(Filter::new(
        "slug",
        Op::Eq,
        slug.clone(),
    )));
    let row = rustango::sql::select_one_row_as_json(pool, &query, &fields)
        .await
        .unwrap()
        .expect("provider row");
    let stored = row["client_secret"].as_str().unwrap();
    assert_ne!(stored, "plain-secret-1764", "stored in the clear");
    assert_eq!(
        EncryptedString::from_db(stored).unwrap(),
        "plain-secret-1764"
    );
    // `auto_now_add` columns were filled.
    assert!(!row["created_at"].is_null(), "{row}");

    let pk = row["id"].to_string();
    let (_, detail) = send(
        pool,
        Method::GET,
        &format!("/rustango_shared_sso_providers/{pk}"),
        String::new(),
    )
    .await;
    assert!(!detail.contains("plain-secret-1764"), "{detail}");
    assert!(!detail.contains(stored), "{detail}");
}

async fn api_keys_are_not_added_through_the_admin(pool: &Pool) {
    let (status, _) = send(pool, Method::GET, "/rustango_api_keys/new", String::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn user_pk(pool: &Pool, name: &str) -> i64 {
    user_named(pool, name).await.id.get().copied().unwrap()
}

/// A non-superuser with `change` cannot move a key onto another user (#2520).
async fn api_key_owner_is_superuser_only(pool: &Pool) {
    use rustango::tenancy::auth_backends::ApiKey;
    let (owner, other) = (unique("kown"), unique("koth"));
    create_user(pool, &owner, "pw-2520-a").await;
    create_user(pool, &other, "pw-2520-b").await;
    let (owner_pk, other_pk) = (user_pk(pool, &owner).await, user_pk(pool, &other).await);
    rustango::tenancy::create_api_key(owner_pk, "k2520", None, pool)
        .await
        .unwrap();
    let key_of = |uid: i64| async move {
        ApiKey::objects()
            .where_(ApiKey::user_id.eq(uid))
            .fetch(pool)
            .await
            .unwrap()
    };
    let pk = key_of(owner_pk).await[0].id.get().copied().unwrap();
    let uri = format!("/rustango_api_keys/{pk}");
    let perms: &[&str] = &["rustango_api_keys.view", "rustango_api_keys.change"];

    let (_, html) = send_as(
        pool,
        Some(perms),
        Method::GET,
        &format!("{uri}/edit"),
        String::new(),
    )
    .await;
    assert!(input_tag(&html, "user_id").contains("readonly"), "{html}");

    let other_s = other_pk.to_string();
    let body = || {
        form(&[
            ("user_id", other_s.as_str()),
            ("label", "moved"),
            ("expires_at", ""),
        ])
    };
    let (status, html) = send_as(pool, Some(perms), Method::POST, &uri, body()).await;
    assert!(status.is_redirection(), "{status}: {html}");
    let kept = key_of(owner_pk).await;
    assert_eq!(kept.len(), 1, "the key moved to another user");
    assert_eq!(kept[0].label, "moved");

    // A superuser still can.
    let (status, html) = send(pool, Method::POST, &uri, body()).await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(key_of(other_pk).await.len(), 1);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        new_user_form_does_not_block_submit,
        admin_created_user_can_log_in,
        a_login_upgrades_a_weak_hash,
        editing_a_user_keeps_or_rotates_the_password,
        admin_created_provider_secret_is_encrypted,
        api_keys_are_not_added_through_the_admin,
        api_key_owner_is_superuser_only,
    ],
}
