//! Single-database admin accounts on every backend: the bare admin's audit
//! source and password change, the account CLI verbs and `audit-cleanup`.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin",
    feature = "testkit"
))]

use std::path::Path;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rustango::admin::AdminUser;
use rustango::audit::AuditLog;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

const PASSWORD: &str = "single-db-admin-pw-1";

#[derive(Model, Debug, Clone)]
#[rustango(table = "sdat_note", audit(track = "body"))]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub body: String,
}

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    rustango::testkit::add_missing_columns::<AdminUser>(pool)
        .await
        .expect("admin user columns");
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(pool)
        .await
        .expect("totp table");
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
}

/// Unique per run: the shared tables keep earlier runs' rows.
fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{n}")
}

async fn seed(pool: &Pool, prefix: &str) -> AdminUser {
    let mut u = AdminUser::new_with_password(unique(prefix), PASSWORD, true).unwrap();
    u.insert_pool(pool).await.expect("seed admin");
    u
}

async fn reload(pool: &Pool, username: &str) -> AdminUser {
    AdminUser::objects()
        .filter("username", username)
        .fetch(pool)
        .await
        .expect("admins")
        .into_iter()
        .next()
        .expect("admin row")
}

fn app(pool: &Pool) -> Router {
    rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_session_auth(rustango::session::SessionSecret::from_bytes(vec![7u8; 32]))
        .build()
}

async fn send(
    app: &Router,
    uri: &str,
    body: String,
    cookie: &str,
) -> (StatusCode, String, Option<String>) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
                .header("x-csrf-token", "t")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let session = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("rustango_admin_session="))
        .map(|v| v.split(';').next().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        session,
    )
}

async fn login(app: &Router, user: &AdminUser) -> String {
    let form = format!("_csrf=t&username={}&password={PASSWORD}", user.username);
    let (status, body, cookie) = send(app, "/login", form, "").await;
    cookie.unwrap_or_else(|| panic!("no session cookie: {status} {body}"))
}

async fn manage(pool: &Pool, args: &[&str]) -> Result<String, rustango::migrate::MigrateError> {
    let mut out = Vec::new();
    let args = args.iter().map(|s| (*s).to_owned());
    rustango::migrate::manage::run_with_writer(pool, Path::new("./migrations"), args, &mut out)
        .await?;
    Ok(String::from_utf8(out).unwrap())
}

fn admin_id(u: &AdminUser) -> i64 {
    *u.id.get().expect("pk")
}

async fn audit_sources(pool: &Pool, table: &str, pk: &str) -> Vec<String> {
    AuditLog::objects()
        .filter("entity_table", table)
        .filter("entity_pk", pk)
        .fetch(pool)
        .await
        .expect("audit rows")
        .into_iter()
        .map(|r| r.source)
        .collect()
}

/// A bare-admin edit records the signed-in user, not `system`.
async fn admin_edit_records_the_signed_in_user(pool: &Pool) {
    let me = seed(pool, "src").await;
    let app = app(pool);
    let cookie = login(&app, &me).await;
    let mut note = Note {
        id: Auto::default(),
        body: "a".into(),
    };
    note.insert_pool(pool).await.expect("note");
    let pk = note.id.get().expect("pk").to_string();
    AuditLog::delete_where("entity_table", "sdat_note", pool)
        .await
        .expect("clear seed audit");

    let (status, body, _) = send(
        &app,
        &format!("/sdat_note/{pk}"),
        "_csrf=t&body=b".into(),
        &cookie,
    )
    .await;
    assert!(status.is_redirection(), "{status}: {body}");
    let sources = audit_sources(pool, "sdat_note", &pk).await;
    assert_eq!(sources, vec![format!("user:{}", admin_id(&me))]);
}

/// The bare change-password form refuses the current password as the new one.
async fn change_password_refuses_the_same_password(pool: &Pool) {
    let me = seed(pool, "same").await;
    let app = app(pool);
    let cookie = login(&app, &me).await;
    let form = format!(
        "_csrf=t&current_password={PASSWORD}&new_password={PASSWORD}&new_password_confirm={PASSWORD}"
    );
    let (status, body, _) = send(&app, "/account/password", form, &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("must differ"), "{body}");
    assert_eq!(
        reload(pool, &me.username).await.password_hash,
        me.password_hash
    );
}

/// `reset-password` and `change-password` replace the hash and audit as `cli`.
async fn password_verbs_replace_the_hash(pool: &Pool) {
    let me = seed(pool, "pw").await;
    let out = manage(
        pool,
        &[
            "reset-password",
            &me.username,
            "--password",
            "Reset-pw-0001",
        ],
    )
    .await
    .expect("reset-password");
    assert!(out.contains("password reset"), "{out}");
    let after = reload(pool, &me.username).await;
    assert!(rustango::passwords::verify("Reset-pw-0001", &after.password_hash).unwrap());

    let wrong = manage(
        pool,
        &[
            "change-password",
            &me.username,
            "--current",
            "nope",
            "--password",
            "Change-pw-0002",
        ],
    )
    .await;
    assert!(wrong.is_err(), "a wrong current password changed it");
    manage(
        pool,
        &[
            "change-password",
            &me.username,
            "--current",
            "Reset-pw-0001",
            "--password",
            "Change-pw-0002",
        ],
    )
    .await
    .expect("change-password");
    let after = reload(pool, &me.username).await;
    assert!(rustango::passwords::verify("Change-pw-0002", &after.password_hash).unwrap());
    let pk = admin_id(&me).to_string();
    assert_eq!(
        audit_sources(pool, "rustango_admin_users", &pk).await,
        ["cli", "cli"]
    );
    let missing = manage(
        pool,
        &[
            "reset-password",
            "no-such-admin-x",
            "--password",
            "Reset-pw-0001",
        ],
    )
    .await;
    assert!(missing.is_err());
}

/// `set-admin-active --off` locks the user out and ends their sessions.
async fn set_admin_active_off_ends_sessions(pool: &Pool) {
    let me = seed(pool, "off").await;
    let app = app(pool);
    let cookie = login(&app, &me).await;
    manage(pool, &["set-admin-active", &me.username, "--off"])
        .await
        .expect("deactivate");
    let after = reload(pool, &me.username).await;
    assert!(!after.active);
    assert!(after.sessions_revoked_at.is_some());
    let (status, _, _) = send(&app, "/account/password", String::new(), &cookie).await;
    assert!(
        status.is_redirection(),
        "a deactivated admin's cookie still works: {status}"
    );

    manage(pool, &["set-admin-active", &me.username, "--on"])
        .await
        .expect("reactivate");
    assert!(reload(pool, &me.username).await.active);
    assert!(manage(pool, &["set-admin-active", &me.username])
        .await
        .is_err());
}

/// `audit-cleanup --days N` drops entries older than N days and keeps newer ones.
async fn audit_cleanup_trims_old_entries(pool: &Pool) {
    let table = unique("sdat_probe_");
    for (pk, age_days) in [("old", 400), ("new", 1)] {
        let mut row = AuditLog {
            id: Auto::default(),
            entity_table: table.clone(),
            entity_pk: pk.into(),
            operation: "update".into(),
            source: "system".into(),
            changes: serde_json::json!({}),
            occurred_at: chrono::Utc::now() - chrono::Duration::days(age_days),
        };
        row.insert_pool(pool).await.expect("audit row");
    }
    let refused = manage(pool, &["audit-cleanup", "--days", "0"]).await;
    assert!(refused.is_err(), "--days 0 ran without --yes");
    assert!(manage(pool, &["audit-cleanup"]).await.is_err());

    let out = manage(pool, &["audit-cleanup", "--days", "365"])
        .await
        .expect("audit-cleanup");
    assert!(out.contains("deleted="), "{out}");
    let left: Vec<String> = AuditLog::objects()
        .filter("entity_table", table.as_str())
        .fetch(pool)
        .await
        .expect("audit rows")
        .into_iter()
        .map(|r| r.entity_pk)
        .collect();
    assert_eq!(left, ["new"]);
}

/// The bare admin turns TOTP off for a current code, and the CLI resets it.
#[cfg(feature = "totp")]
async fn totp_off_by_code_and_by_cli(pool: &Pool) {
    use rustango::admin::totp_store;
    use rustango::totp::{self, TotpSecret};
    let me = seed(pool, "totp").await;
    let uid = admin_id(&me);
    let app = app(pool);
    let cookie = login(&app, &me).await;
    let secret = TotpSecret::generate();
    totp_store::start_enrollment(pool, uid, &secret)
        .await
        .expect("enroll");
    totp_store::confirm(pool, uid).await.expect("confirm");

    let (status, body, _) = send(
        &app,
        "/account/totp/disable",
        "_csrf=t&totp_code=000000".into(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        totp_store::device(pool, uid).await.is_some(),
        "a wrong code turned it off"
    );

    let code = totp::generate(&secret, 30, 6);
    let (status, body, _) = send(
        &app,
        "/account/totp/disable",
        format!("_csrf=t&totp_code={code}"),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(totp_store::device(pool, uid).await.is_none(), "{body}");
    let pk = uid.to_string();
    let user_src = format!("user:{uid}");
    assert!(audit_sources(pool, "rustango_admin_users", &pk)
        .await
        .contains(&user_src));

    totp_store::start_enrollment(pool, uid, &secret)
        .await
        .expect("enroll");
    totp_store::confirm(pool, uid).await.expect("confirm");
    manage(pool, &["reset-admin-totp", &me.username])
        .await
        .expect("reset-admin-totp");
    assert!(totp_store::device(pool, uid).await.is_none());
    // A session opened with the lost device ends with it.
    let (status, _, _) = send(&app, "/account/password", String::new(), &cookie).await;
    assert!(
        status.is_redirection(),
        "the old session survived: {status}"
    );
    assert!(audit_sources(pool, "rustango_admin_users", &pk)
        .await
        .contains(&"cli".to_owned()));

    // A retry after the device is gone still ends the sessions.
    let cookie = login(&app, &me).await;
    let (status, _, _) = send(&app, "/account/password", String::new(), &cookie).await;
    assert!(
        !status.is_redirection(),
        "the new session is dead: {status}"
    );
    manage(pool, &["reset-admin-totp", &me.username])
        .await
        .expect("reset-admin-totp again");
    let (status, _, _) = send(&app, "/account/password", String::new(), &cookie).await;
    assert!(
        status.is_redirection(),
        "a retry left the session: {status}"
    );
}

#[cfg(not(feature = "totp"))]
async fn totp_off_by_code_and_by_cli(_: &Pool) {
    panic!("this scenario needs the `totp` feature");
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        admin_edit_records_the_signed_in_user,
        change_password_refuses_the_same_password,
        password_verbs_replace_the_hash,
        set_admin_active_off_ends_sessions,
        audit_cleanup_trims_old_entries,
        totp_off_by_code_and_by_cli,
    ],
);
