//! Tenant users and SSO providers created and edited through the admin,
//! on every backend (#1763, #1764).

#![cfg(all(feature = "admin", feature = "tenancy", feature = "admin-sso"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::casts::{CastValue as _, EncryptedString};
use rustango::core::{Column as _, Filter, Op, SelectQuery, WhereExpr};
use rustango::sql::{FetcherPool as _, Pool, UpdaterPool as _};
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
    // A narrower build may have made these tables without `email`.
    rustango::testkit::add_missing_columns::<User>(pool)
        .await
        .expect("user columns");
    rustango::testkit::add_missing_columns::<rustango::admin::AdminUser>(pool)
        .await
        .expect("admin user columns");
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
    let registry = [
        "/rustango_shared_sso_providers",
        "/rustango_operators",
        "/rustango_orgs",
    ];
    let app = if registry.iter().any(|t| uri.starts_with(t)) {
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
    create_user(pool, &name, "pw-1763-older").await;
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
    assert!(logs_in(pool, &name, "pw-1763-older").await);
    assert!(user_named(pool, &name).await.password_changed_at.is_none());

    // A new one replaces it and ends older sessions.
    let (status, body) = send(pool, Method::POST, &uri, edit("pw-1763-newer")).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert!(logs_in(pool, &name, "pw-1763-newer").await);
    assert!(!logs_in(pool, &name, "pw-1763-older").await);
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

/// Deactivating a user in the admin ends its sessions and JWT refresh.
async fn deactivating_a_user_ends_its_sessions(pool: &Pool) {
    let name = unique("off");
    create_user(pool, &name, "pw-1412-strong").await;
    let pk = user_named(pool, &name).await.id.get().copied().unwrap();
    let uri = format!("/rustango_users/{pk}");
    // An unchecked checkbox is absent from the form.
    let off = form(&[
        ("username", name.as_str()),
        ("password_hash", ""),
        ("data", ""),
    ]);
    let (status, body) = send(pool, Method::POST, &uri, off).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let user = user_named(pool, &name).await;
    assert!(!user.active);
    let stamp = user.sessions_revoked_at.expect("sessions not revoked");
    assert!(
        stamp >= chrono::Utc::now() - chrono::Duration::seconds(5),
        "{stamp}"
    );

    // Reactivating leaves the stamp alone.
    let on = form(&[
        ("username", name.as_str()),
        ("password_hash", ""),
        ("active", "true"),
        ("data", ""),
    ]);
    let (status, body) = send(pool, Method::POST, &uri, on).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let user = user_named(pool, &name).await;
    assert!(user.active);
    assert_eq!(user.sessions_revoked_at, Some(stamp));
}

/// The registry admin shows neither an operator's hash nor a
/// tenant's database URL; neither model has a `password` widget.
async fn registry_admin_hides_credentials(pool: &Pool) {
    use rustango::tenancy::{Operator, Org};
    let mut op = Operator {
        id: rustango::sql::Auto::default(),
        username: unique("op"),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$HASH1158MARK".into(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(pool).await.expect("seed operator");
    let mut org = Org {
        slug: unique("o"),
        database_url: Some("postgres://app:URL1158MARK@db/t".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(pool).await.expect("seed org");
    let op_pk = op.id.get().copied().unwrap();
    let org_pk = org.id.get().copied().unwrap();
    for uri in [
        format!("/rustango_operators?q={}", op.username),
        format!("/rustango_operators/{op_pk}"),
        format!("/rustango_orgs?q={}", org.slug),
        format!("/rustango_orgs/{org_pk}"),
    ] {
        let (status, html) = send(pool, Method::GET, &uri, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {html}");
        assert!(
            html.contains(&op.username) || html.contains(&org.slug),
            "{uri} lacks the row: {html}"
        );
        assert!(!html.contains("HASH1158MARK"), "{uri} shows the hash");
        assert!(!html.contains("URL1158MARK"), "{uri} shows the URL");
    }
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
    create_user(pool, &owner, "pw-2520-owner-a").await;
    create_user(pool, &other, "pw-2520-owner-b").await;
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

/// A non-superuser with `add`/`change` cannot grant superuser or take over
/// an account through its password or email (#2521).
async fn user_superuser_fields_are_superuser_only(pool: &Pool) {
    let perms: &[&str] = &[
        "rustango_users.view",
        "rustango_users.add",
        "rustango_users.change",
    ];
    // The add form locks the grant and the email, not the password.
    let (_, html) = send_as(
        pool,
        Some(perms),
        Method::GET,
        "/rustango_users/new",
        String::new(),
    )
    .await;
    assert!(
        input_tag(&html, "is_superuser").contains("readonly"),
        "{html}"
    );
    assert!(input_tag(&html, "email").contains("readonly"), "{html}");
    assert!(
        !input_tag(&html, "password_hash").contains("readonly"),
        "{html}"
    );

    let name = unique("su");
    let email = format!("{name}@example.com");
    let (status, html) = send_as(
        pool,
        Some(perms),
        Method::POST,
        "/rustango_users",
        form(&[
            ("username", name.as_str()),
            ("password_hash", "pw-2521-owner"),
            ("email", email.as_str()),
            ("is_superuser", "true"),
            ("active", "true"),
            ("data", ""),
        ]),
    )
    .await;
    assert!(status.is_redirection(), "{status}: {html}");
    let user = user_named(pool, &name).await;
    assert!(!user.is_superuser, "a non-superuser created a superuser");
    assert!(user.email.is_none(), "a non-superuser set an email on add");
    assert!(logs_in(pool, &name, "pw-2521-owner").await);

    let pk = user.id.get().copied().unwrap();
    let uri = format!("/rustango_users/{pk}");
    let (_, html) = send_as(
        pool,
        Some(perms),
        Method::GET,
        &format!("{uri}/edit"),
        String::new(),
    )
    .await;
    for field in ["is_superuser", "password_hash", "email"] {
        assert!(
            input_tag(&html, field).contains("readonly"),
            "{field}: {html}"
        );
    }
    let renamed = format!("{name}r");
    let (status, html) = send_as(
        pool,
        Some(perms),
        Method::POST,
        &uri,
        form(&[
            ("username", renamed.as_str()),
            ("password_hash", "pw-2521-taken"),
            ("email", "taken-2521@example.com"),
            ("is_superuser", "true"),
            ("active", "true"),
            ("data", ""),
        ]),
    )
    .await;
    assert!(status.is_redirection(), "{status}: {html}");
    let user = user_named(pool, &renamed).await;
    assert!(!user.is_superuser, "a non-superuser granted superuser");
    assert!(user.email.is_none(), "a non-superuser set the email");
    assert!(logs_in(pool, &renamed, "pw-2521-owner").await);
    assert!(!logs_in(pool, &renamed, "pw-2521-taken").await);
}

/// A non-superuser cannot edit or delete a superuser's row (#2521).
async fn superuser_rows_are_superuser_only(pool: &Pool) {
    let name = unique("root");
    let (status, html) = send(
        pool,
        Method::POST,
        "/rustango_users",
        form(&[
            ("username", name.as_str()),
            ("password_hash", "pw-2521-root"),
            ("is_superuser", "true"),
            ("active", "true"),
            ("data", ""),
        ]),
    )
    .await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert!(
        user_named(pool, &name).await.is_superuser,
        "a superuser can grant it"
    );
    let uri = format!("/rustango_users/{}", user_pk(pool, &name).await);
    let perms: &[&str] = &[
        "rustango_users.view",
        "rustango_users.change",
        "rustango_users.delete",
    ];
    let deactivate = form(&[("username", name.as_str()), ("data", "")]);
    let (status, _) = send_as(pool, Some(perms), Method::POST, &uri, deactivate).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send_as(
        pool,
        Some(perms),
        Method::POST,
        &format!("{uri}/delete"),
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        user_named(pool, &name).await.active,
        "the superuser row changed"
    );
}

/// The admin form holds every account password to the built-in policy.
async fn admin_password_fields_apply_the_policy(pool: &Pool) {
    const WEAK: &str = "a";
    let refused = |status: StatusCode, html: &str| {
        assert!(!status.is_redirection(), "{status}: {html}");
        assert!(html.contains("Password rejected"), "{html}");
    };
    // Tenant user: create and edit.
    let name = unique("weak");
    let (status, html) = create_user(pool, &name, WEAK).await;
    refused(status, &html);
    create_user(pool, &name, "pw-policy-strong").await;
    let pk = user_pk(pool, &name).await;
    let edit = form(&[
        ("username", name.as_str()),
        ("password_hash", WEAK),
        ("active", "true"),
        ("data", ""),
    ]);
    let (status, html) = send(pool, Method::POST, &format!("/rustango_users/{pk}"), edit).await;
    refused(status, &html);
    assert!(logs_in(pool, &name, "pw-policy-strong").await);

    // Operator, through the registry admin.
    let op = unique("weakop");
    let op_id = seed_operator(pool, &op).await;
    let (status, html) = send(
        pool,
        Method::POST,
        &format!("/rustango_operators/{op_id}"),
        form(&[
            ("username", op.as_str()),
            ("password_hash", WEAK),
            ("created_at", "2024-01-01T00:00"),
            ("active", "true"),
        ]),
    )
    .await;
    refused(status, &html);
}

async fn seed_operator(pool: &Pool, name: &str) -> i64 {
    let mut op = rustango::tenancy::Operator {
        id: rustango::sql::Auto::default(),
        username: name.to_owned(),
        password_hash: "x".into(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(pool).await.expect("seed operator");
    op.id.get().copied().unwrap()
}

async fn active_operators(pool: &Pool) -> Vec<i64> {
    use rustango::tenancy::Operator;
    Operator::objects()
        .where_(Operator::active.eq(true))
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.id.get().copied().unwrap())
        .collect()
}

/// The registry admin cannot deactivate or delete the last active operator.
async fn admin_keeps_the_last_active_operator(pool: &Pool) {
    let (a, b, c) = (unique("la"), unique("lb"), unique("lc"));
    // Earlier rows are shared: only the two seeded here stay active.
    use rustango::tenancy::Operator;
    Operator::objects()
        .where_(Operator::active.eq(true))
        .update()
        .set_typed(Operator::active.set(false))
        .execute_pool(pool)
        .await
        .unwrap();
    let (a_id, b_id) = (seed_operator(pool, &a).await, seed_operator(pool, &b).await);
    let at = "2024-01-01T00:00";
    let off = |name: &str| {
        form(&[
            ("username", name),
            ("password_hash", ""),
            ("created_at", at),
        ])
    };
    // One of two: allowed.
    let (status, html) = send(
        pool,
        Method::POST,
        &format!("/rustango_operators/{a_id}"),
        off(&a),
    )
    .await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(active_operators(pool).await, [b_id]);
    // The last one: neither an edit nor a delete.
    let uri = format!("/rustango_operators/{b_id}");
    let (status, html) = send(pool, Method::POST, &uri, off(&b)).await;
    assert!(!status.is_redirection(), "{status}: {html}");
    assert!(html.contains("last active operator"), "{html}");
    let (status, _) = send(pool, Method::POST, &format!("{uri}/delete"), String::new()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(active_operators(pool).await, [b_id]);
    let c_id = seed_operator(pool, &c).await;
    // Deleting one of two, and editing the last without deactivating, work.
    let (status, _) = send(
        pool,
        Method::POST,
        &format!("/rustango_operators/{c_id}/delete"),
        String::new(),
    )
    .await;
    assert!(status.is_redirection(), "{status}");
    let on = form(&[
        ("username", b.as_str()),
        ("password_hash", ""),
        ("created_at", at),
        ("active", "true"),
    ]);
    let (status, html) = send(pool, Method::POST, &uri, on).await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(active_operators(pool).await, [b_id]);
}

/// The console and the admin deactivating the last two operators at once
/// leave one active: both lock the active rows in id order.
async fn console_and_admin_race_keeps_one_active(pool: &Pool) {
    use rustango::tenancy::Operator;
    let at = "2024-01-01T00:00";
    for round in 0..10 {
        Operator::objects()
            .where_(Operator::active.eq(true))
            .update()
            .set_typed(Operator::active.set(false))
            .execute_pool(pool)
            .await
            .unwrap();
        let (a, b) = (
            unique(&format!("ra{round}x")),
            unique(&format!("rb{round}x")),
        );
        let a_id = seed_operator(pool, &a).await;
        let b_id = seed_operator(pool, &b).await;
        let mut b_op = Operator::objects()
            .where_(Operator::id.eq(b_id))
            .fetch(pool)
            .await
            .unwrap()
            .remove(0);
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let console = {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                rustango::tenancy::operators::set_active(&pool, &mut b_op, false, None)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        };
        let admin = {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            let body = form(&[
                ("username", a.as_str()),
                ("password_hash", ""),
                ("created_at", at),
            ]);
            tokio::spawn(async move {
                barrier.wait().await;
                send(
                    &pool,
                    Method::POST,
                    &format!("/rustango_operators/{a_id}"),
                    body,
                )
                .await
                .0
            })
        };
        let (c, s) = (console.await.unwrap(), admin.await.unwrap());
        let left = active_operators(pool).await;
        assert_eq!(left.len(), 1, "round {round}: console {c:?}, admin {s}");
    }
}

/// A link row pointing `user_id` at `source`'s user table; its subject.
async fn seed_link(pool: &Pool, source: &str, user_id: i64) -> String {
    use rustango::sso::SsoLink;
    let subject = unique("sub");
    SsoLink {
        id: rustango::sql::Auto::default(),
        provider_source: source.into(),
        provider_id: 1,
        issuer: "oidc".into(),
        subject: subject.clone(),
        key_sha256: rustango::sso::link::key_sha256("oidc", &subject),
        user_id,
        created_at: rustango::sql::Auto::default(),
        provisioned: false,
        last_used_at: None,
    }
    .insert_pool(pool)
    .await
    .unwrap();
    subject
}

/// The `provider_source` of each link with these subjects, in order.
async fn link_sources(pool: &Pool, subjects: &[&str]) -> Vec<Option<String>> {
    let mut out = Vec::new();
    for subject in subjects {
        let rows = rustango::sso::SsoLink::objects()
            .filter("subject", (*subject).to_owned())
            .fetch(pool)
            .await
            .unwrap();
        out.push(rows.into_iter().next().map(|l| l.provider_source));
    }
    out
}

/// Deleting a user in the admin deletes its links, so a later user given
/// the same id does not sign in through them; an `admin` link points into
/// the admin's own user table and stays.
async fn deleting_a_user_deletes_its_links(pool: &Pool) {
    let name = unique("gone");
    let (status, body) = create_user(pool, &name, "pw-links-gone").await;
    assert!(status.is_redirection(), "{status}: {body}");
    let id = user_pk(pool, &name).await;
    let tenant = seed_link(pool, "tenant", id).await;
    let shared = seed_link(pool, "shared", id).await;
    let app = seed_link(pool, "app", id).await;
    let admin = seed_link(pool, "admin", id).await;
    let (status, body) = send(
        pool,
        Method::POST,
        &format!("/rustango_users/{id}/delete"),
        String::new(),
    )
    .await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(
        link_sources(pool, &[&tenant, &shared, &app, &admin]).await,
        [None, None, None, Some("admin".to_owned())]
    );
}

/// POST a new link through a tenant (`tenant_db`) or single-database admin;
/// whether the row was written.
async fn admin_adds_link(pool: &Pool, tenant_db: bool, source: &str) -> bool {
    let subject = unique("asub");
    let builder = rustango::admin::Builder::new(pool.clone()).admin_prefix("");
    let app = if tenant_db {
        builder.tenant_mode()
    } else {
        builder
    }
    .build();
    let body = form(&[
        ("provider_source", source),
        ("provider_id", "1"),
        ("issuer", "oidc"),
        ("subject", &subject),
        ("user_id", "1"),
    ]);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/rustango_sso_links")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    link_sources(pool, &[&subject]).await[0].is_some()
}

/// An `admin` link outside the bare admin's own database would sign someone
/// into the bare admin; the admin writes only the sources its database holds.
async fn admin_link_sources_follow_the_database(pool: &Pool) {
    for (tenant_db, source, written) in [
        (true, "admin", false),
        (true, "tenant", true),
        (true, "shared", true),
        (true, "app", true),
        (true, "bogus", false),
        (false, "admin", true),
        (false, "tenant", true),
        (false, "shared", false),
    ] {
        assert_eq!(
            admin_adds_link(pool, tenant_db, source).await,
            written,
            "tenant_db={tenant_db} {source}"
        );
    }
    // An edit cannot turn a tenant link into an admin one either.
    let subject = seed_link(pool, "tenant", 1).await;
    let id = rustango::sso::SsoLink::objects()
        .filter("subject", subject.clone())
        .fetch(pool)
        .await
        .unwrap()[0]
        .id
        .get()
        .copied()
        .unwrap();
    let body = form(&[
        ("provider_source", "admin"),
        ("provider_id", "1"),
        ("issuer", "oidc"),
        ("subject", &subject),
        ("user_id", "2"),
    ]);
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .tenant_mode()
        .build();
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/rustango_sso_links/{id}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        link_sources(pool, &[&subject]).await,
        [Some("tenant".to_owned())]
    );
    let user_of = |subject: String| async move {
        rustango::sso::SsoLink::objects()
            .filter("subject", subject)
            .fetch(pool)
            .await
            .unwrap()[0]
            .user_id
    };
    assert_eq!(
        user_of(subject.clone()).await,
        1,
        "the refused edit wrote nothing"
    );
    // The same edit with an allowed source saves, so the refusal above is the rule.
    let body = form(&[
        ("provider_source", "tenant"),
        ("provider_id", "1"),
        ("issuer", "oidc"),
        ("subject", &subject),
        ("user_id", "2"),
    ]);
    let resp = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .tenant_mode()
        .build()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/rustango_sso_links/{id}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_redirection(), "{}", resp.status());
    assert_eq!(user_of(subject.clone()).await, 2, "the allowed edit saved");

    // A stored `admin` link in a tenant database: the refusal says the stored value is wrong.
    let subject = seed_link(pool, "admin", 1).await;
    let id = rustango::sso::SsoLink::objects()
        .filter("subject", subject.clone())
        .fetch(pool)
        .await
        .unwrap()[0]
        .id
        .get()
        .copied()
        .unwrap();
    let body = form(&[
        ("provider_source", "admin"),
        ("provider_id", "1"),
        ("issuer", "oidc"),
        ("subject", &subject),
        ("user_id", "2"),
    ]);
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .tenant_mode()
        .build();
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/rustango_sso_links/{id}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let html = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("stored provider_source"), "{html}");
    let rows = rustango::sso::SsoLink::objects()
        .filter("subject", subject)
        .fetch(pool)
        .await
        .unwrap();
    assert_eq!(rows[0].user_id, 1, "the edit is not saved");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        console_and_admin_race_keeps_one_active,
        admin_keeps_the_last_active_operator,
        new_user_form_does_not_block_submit,
        admin_created_user_can_log_in,
        a_login_upgrades_a_weak_hash,
        editing_a_user_keeps_or_rotates_the_password,
        deactivating_a_user_ends_its_sessions,
        registry_admin_hides_credentials,
        admin_created_provider_secret_is_encrypted,
        api_keys_are_not_added_through_the_admin,
        api_key_owner_is_superuser_only,
        user_superuser_fields_are_superuser_only,
        superuser_rows_are_superuser_only,
        admin_password_fields_apply_the_policy,
        deleting_a_user_deletes_its_links,
        admin_link_sources_follow_the_database,
    ],
}
