#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Operator console errors log the driver text and show an opaque
//! message (#2034).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::operator_console::{router_with_brand_storage, SessionSecret};
use rustango::tenancy::TenantPools;
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// The driver's own words for the fault each test causes.
const DRIVER_TEXT: &str = "no such table";

struct Booted {
    app: axum::Router,
    cookie: String,
    raw: sqlx::SqlitePool,
    _tmp: tempfile::TempDir,
}

async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool.clone()));
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
    let username = format!(
        "op-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    );
    let password = "letmein";
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash(password).unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&registry).await.expect("seed operator");

    let storage: rustango::storage::BoxedStorage = Arc::new(rustango::storage::LocalStorage::new(
        tmp.path().join("brand"),
    ));
    let app = router_with_brand_storage(
        registry,
        Some(pools.clone().into_invalidator()),
        SessionSecret::from_env_or_random(),
        storage,
    );
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
        raw: pool,
        _tmp: tmp,
    }
}

/// Make every `rustango_orgs` query fail inside the driver. Operators
/// live in their own table, so the session still authenticates.
async fn break_org_table(raw: &sqlx::SqlitePool) {
    sqlx::query("DROP TABLE rustango_orgs")
        .execute(raw)
        .await
        .expect("drop rustango_orgs");
}

#[tokio::test]
async fn a_failed_tenant_probe_withholds_the_driver_text() {
    let b = boot().await;
    break_org_table(&b.raw).await;
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/acme/test-connection")
                .header("x-csrf-token", "t")
                .header(header::COOKIE, format!("rustango_csrf=t; {}", b.cookie))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body =
        String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    assert!(!body.contains(DRIVER_TEXT), "driver text leaked: {body}");
    assert!(body.contains("Could not read the tenant"), "{body}");
}

#[tokio::test]
async fn a_failed_branding_save_withholds_the_driver_text() {
    let b = boot().await;
    break_org_table(&b.raw).await;
    let boundary = "b";
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--b\r\nContent-Disposition: form-data; name=\"logo\"; filename=\"logo\"\r\n\
          Content-Type: image/png\r\n\r\npng\r\n--b--\r\n",
    );
    let resp = b
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs/acme/edit/branding")
                .header("x-csrf-token", "t")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(header::COOKIE, format!("rustango_csrf=t; {}", b.cookie))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .expect("redirect back to the form")
        .to_str()
        .unwrap()
        .to_owned();
    let shown = rustango::url_codec::url_decode(&location);
    assert!(!shown.contains(DRIVER_TEXT), "driver text leaked: {shown}");
    assert!(shown.contains("Could not save the branding"), "{shown}");
}
