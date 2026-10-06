#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! The operator console nested under a path prefix keeps its links and
//! redirects under that prefix (#2007).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::operator_console::{router_full, SessionSecret};
use rustango::tenancy::provision::Provisioner;
use rustango::tenancy::{Org, TenantPools};
use tower::ServiceExt;

const PREFIX: &str = "/ops";

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
    username: String,
    slug: String,
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
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: rustango::tenancy::password::hash("letmein").unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(&registry).await.expect("seed operator");

    // A stored logo, so the edit page renders a `/__brand__/` preview.
    let slug = unique("acme");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: slug.clone(),
        storage_mode: "schema".into(),
        backend_kind: "sqlite".into(),
        database_url: None,
        schema_name: Some(slug.clone()),
        host_pattern: Some(format!("{slug}.example.com")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: Some("logo.png".into()),
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&registry).await.expect("seed org");

    let provisioner = Provisioner::new(pools.clone(), url.clone(), migrations.path()).erased();
    let storage: rustango::storage::BoxedStorage = Arc::new(rustango::storage::LocalStorage::new(
        tmp.path().join("brand"),
    ));
    let console = router_full(
        registry,
        Some(pools.clone().into_invalidator()),
        Some(provisioner),
        SessionSecret::from_env_or_random(),
        storage,
        Some(SessionSecret::from_env_or_random()),
        "/_impersonation_handoff".into(),
    );
    Booted {
        app: axum::Router::new().nest(PREFIX, console),
        username,
        slug,
        _tmp: tmp,
        _migrations: migrations,
    }
}

async fn send(app: &axum::Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.unwrap()
}

fn location(resp: &axum::response::Response) -> String {
    resp.headers()
        .get(header::LOCATION)
        .expect("a redirect")
        .to_str()
        .unwrap()
        .to_owned()
}

/// Sign in through the prefixed login form; returns the session cookie
/// and where the login sent the browser.
async fn login(b: &Booted, next: &str) -> (String, String) {
    let resp = send(
        &b.app,
        Request::builder()
            .method("POST")
            .uri(format!("{PREFIX}/login"))
            .header("cookie", "rustango_csrf=t")
            .header("x-csrf-token", "t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username={}&password=letmein&next={next}",
                b.username
            )))
            .unwrap(),
    )
    .await;
    let cookie = resp
        .headers()
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    (cookie, location(&resp))
}

async fn page(b: &Booted, cookie: &str, path: &str) -> String {
    let resp = send(
        &b.app,
        Request::builder()
            .uri(format!("{PREFIX}{path}"))
            .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(resp.status().is_success(), "{path}: {}", resp.status());
    let html =
        String::from_utf8(to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec()).unwrap();
    // Tera escapes `/` inside a value; the browser reads it back.
    html.replace("&#x2F;", "/")
}

/// Every root-relative `href`, `action` and `src` in `html` that does
/// not start with the mount prefix.
fn unprefixed_links(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in [
        "href=\"",
        "action=\"",
        "src=\"",
        "fetch(\"",
        "EventSource(\"",
    ] {
        for (i, _) in html.match_indices(attr) {
            let value: String = html[i + attr.len()..]
                .chars()
                .take_while(|c| *c != '"')
                .collect();
            if value.starts_with('/') && !value.starts_with("//") {
                let ok = value == PREFIX
                    || value.starts_with(&format!("{PREFIX}/"))
                    || value.starts_with(&format!("{PREFIX}?"));
                if !ok {
                    out.push(format!("{attr}{value}"));
                }
            }
        }
    }
    out
}

#[tokio::test]
async fn an_unauthenticated_request_is_sent_to_the_prefixed_login() {
    let b = boot().await;
    let resp = send(
        &b.app,
        Request::builder()
            .uri(format!("{PREFIX}/orgs"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let loc = location(&resp);
    assert!(loc.starts_with("/ops/login?next="), "{loc}");
}

#[tokio::test]
async fn login_redirects_under_the_prefix() {
    let b = boot().await;
    let (_, loc) = login(&b, "%2Forgs").await;
    assert_eq!(loc, "/ops/orgs");
}

#[tokio::test]
async fn every_page_links_under_the_prefix() {
    let b = boot().await;
    let (cookie, _) = login(&b, "%2F").await;
    let login_page = send(
        &b.app,
        Request::builder()
            .uri(format!("{PREFIX}/login"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let login_html = String::from_utf8(
        to_bytes(login_page.into_body(), 1 << 22)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
    .replace("&#x2F;", "/");
    let mut bad = unprefixed_links(&login_html);
    let slug = b.slug.clone();
    for path in [
        "".to_owned(),
        "/orgs".to_owned(),
        "/operators".to_owned(),
        "/audit".to_owned(),
        "/change-password".to_owned(),
        "/orgs/new".to_owned(),
        "/orgs/provision".to_owned(),
        format!("/orgs/{slug}/edit"),
        format!("/orgs/{slug}/hosts"),
    ] {
        let html = page(&b, &cookie, &path).await;
        // Scripts build their fetch URLs from this.
        if !html.contains(r#"<meta name="console-prefix" content="/ops">"#) {
            bad.push(format!("{path}: no console-prefix meta"));
        }
        bad.extend(
            unprefixed_links(&html)
                .into_iter()
                .map(|l| format!("{path}: {l}")),
        );
    }
    assert!(bad.is_empty(), "root links under {PREFIX}: {bad:#?}");
}

#[tokio::test]
async fn a_form_redirect_stays_under_the_prefix() {
    let b = boot().await;
    let (cookie, _) = login(&b, "%2F").await;
    let resp = send(
        &b.app,
        Request::builder()
            .method("POST")
            .uri(format!("{PREFIX}/orgs/{}/hosts/add", b.slug))
            .header(header::COOKIE, format!("rustango_csrf=t; {cookie}"))
            .header("x-csrf-token", "t")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("hostname=extra.example.com"))
            .unwrap(),
    )
    .await;
    let loc = location(&resp);
    assert!(
        loc.starts_with(&format!("/ops/orgs/{}/hosts", b.slug)),
        "{loc}"
    );
}
