//! Org and OrgHost writes through the generic admin claim their host,
//! path prefix and port like the console and CLI do.

#![cfg(all(feature = "admin", feature = "tenancy", feature = "testkit"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::org_edit::{apply, OrgPatch};
use rustango::tenancy::{Org, OrgHost, OrgResolver as _, RegisteredHostResolver, TenancyError};
use rustango::tri_dialect_test;
use tower::ServiceExt;

// Extra hosts edited inside the Org page.
rustango::register_admin_inline!(
    parent = "rustango_orgs",
    child = "rustango_org_hosts",
    fk = "org_id",
    fields = &["hostname", "enabled"],
);

async fn setup(pool: &Pool) {
    // Shared registry tables: create what is missing, never drop them.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

/// Unique per run and backend: the shared tables keep earlier rows.
fn name(pool: &Pool, tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("adm-{tag}-{}-{n}", pool.dialect().name())
}

/// A port nobody else in this run uses.
fn port(pool: &Pool, i: u32) -> i32 {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_micros()
        % 500;
    let backend = match pool.dialect().name() {
        "postgres" => 0,
        "mysql" => 1,
        _ => 2,
    };
    i32::try_from(1_000 + backend * 6_000 + run * 10 + i).unwrap()
}

async fn send(pool: &Pool, method: Method, uri: &str, body: String) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .registry_mode()
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

async fn org(pool: &Pool, slug: &str) -> i64 {
    let mut org = Org {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        ..rustango::testkit::org()
    };
    org.save_pool(pool).await.expect("insert org");
    *org.id.get().expect("org id")
}

async fn org_named(pool: &Pool, slug: &str) -> Option<Org> {
    Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await
        .expect("fetch org")
        .into_iter()
        .next()
}

/// The admin's Org form, routing columns as given (empty clears).
fn org_form(slug: &str, host: &str, prefix: &str, port: &str) -> String {
    [
        ("slug", slug),
        ("display_name", slug),
        ("storage_mode", "database"),
        ("backend_kind", "sqlite"),
        ("database_url", "sqlite%3A%3Amemory%3A"),
        ("host_pattern", host),
        ("path_prefix", prefix),
        ("port", port),
        ("active", "true"),
        ("created_at", "2026-01-01T00:00"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", v.replace('/', "%2F")))
    .collect::<Vec<_>>()
    .join("&")
}

/// A tenant holding a host, a prefix and a port.
async fn owner(pool: &Pool) -> (String, String, i32) {
    let slug = name(pool, "owner");
    org(pool, &slug).await;
    let host = format!("{slug}.example.test");
    let prefix = format!("/{slug}");
    let port = port(pool, 0);
    let patch = OrgPatch {
        host_pattern: Some(host.clone()),
        path_prefix: Some(prefix.clone()),
        port: Some(port.to_string()),
        ..OrgPatch::default()
    };
    apply(pool, &slug, &patch).await.expect("owner routing");
    (host, prefix, port)
}

fn refused(status: StatusCode, html: &str, what: &str) {
    assert_eq!(status, StatusCode::OK, "{what}: {html}");
    assert!(html.contains("already used"), "{what} was not refused: {html}");
}

async fn admin_org_edit_refuses_another_tenants_routing(pool: &Pool) {
    let (host, prefix, port) = owner(pool).await;
    let slug = name(pool, "edit");
    let id = org(pool, &slug).await;
    let uri = format!("/rustango_orgs/{id}");
    for (what, form) in [
        ("host", org_form(&slug, &host, "", "")),
        ("prefix", org_form(&slug, "", &prefix, "")),
        ("port", org_form(&slug, "", "", &port.to_string())),
    ] {
        let (status, html) = send(pool, Method::POST, &uri, form).await;
        refused(status, &html, what);
    }
    let row = org_named(pool, &slug).await.unwrap();
    assert_eq!((row.host_pattern, row.path_prefix, row.port), (None, None, None));
}

async fn admin_org_create_refuses_a_taken_host(pool: &Pool) {
    let (host, prefix, port) = owner(pool).await;
    let slug = name(pool, "create");
    for (what, form) in [
        ("host", org_form(&slug, &host, "", "")),
        ("prefix", org_form(&slug, "", &prefix, "")),
        ("port", org_form(&slug, "", "", &port.to_string())),
    ] {
        let (status, html) = send(pool, Method::POST, "/rustango_orgs", form).await;
        refused(status, &html, what);
        assert!(org_named(pool, &slug).await.is_none(), "{what}: created");
    }
}

async fn admin_host_create_refuses_a_base_host(pool: &Pool) {
    let (host, _, _) = owner(pool).await;
    let id = org(pool, &name(pool, "extra")).await;
    let form = format!("org_id={id}&hostname={}&enabled=true", host.to_uppercase());
    let (status, html) = send(pool, Method::POST, "/rustango_org_hosts", form).await;
    refused(status, &html, "extra host");
    let rows: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::org_id.eq(id))
        .fetch(pool)
        .await
        .unwrap();
    assert!(rows.is_empty(), "{rows:?}");
}

/// A tenant's own values are not a clash, and a host is stored normalized.
async fn admin_org_edit_keeps_and_normalizes_its_own_routing(pool: &Pool) {
    let slug = name(pool, "own");
    let id = org(pool, &slug).await;
    let host = format!("{slug}.example.test");
    let port = port(pool, 1).to_string();
    let uri = format!("/rustango_orgs/{id}");
    let prefix = format!("/{slug}");
    let form = org_form(&slug, &host.to_uppercase(), &prefix, &port);
    let (status, html) = send(pool, Method::POST, &uri, form.clone()).await;
    assert!(status.is_redirection(), "{status}: {html}");
    let (status, html) = send(pool, Method::POST, &uri, form).await;
    assert!(status.is_redirection(), "resaving clashed with itself: {status}: {html}");
    let row = org_named(pool, &slug).await.unwrap();
    assert_eq!(row.host_pattern.as_deref(), Some(host.as_str()));
    assert_eq!(row.path_prefix.as_deref(), Some(prefix.as_str()));
}

/// The admin and the CLI take one free prefix at once: only one gets it.
async fn admin_and_cli_racing_for_a_prefix_one_wins(pool: &Pool) {
    let (a, b) = (name(pool, "race-a"), name(pool, "race-b"));
    let a_id = org(pool, &a).await;
    org(pool, &b).await;
    let mut wins = (0, 0);
    for i in 0..10 {
        let prefix = format!("/{a}-{i}");
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let admin = {
            let (pool, barrier, form) = (pool.clone(), barrier.clone(), org_form(&a, "", &prefix, ""));
            tokio::spawn(async move {
                barrier.wait().await;
                send(&pool, Method::POST, &format!("/rustango_orgs/{a_id}"), form).await
            })
        };
        let cli = {
            let (pool, barrier, b) = (pool.clone(), barrier.clone(), b.clone());
            let patch = OrgPatch {
                path_prefix: Some(prefix.clone()),
                ..OrgPatch::default()
            };
            tokio::spawn(async move {
                barrier.wait().await;
                apply(&pool, &b, &patch).await
            })
        };
        let ((status, html), cli) = (admin.await.unwrap(), cli.await.unwrap());
        let admin_won = status.is_redirection();
        let cli_refused =
            matches!(&cli, Err(TenancyError::Validation(m)) if m.contains("already used"));
        if admin_won {
            assert!(cli_refused, "round {i}: both got `{prefix}`: {cli:?}");
            wins.0 += 1;
        } else {
            refused(status, &html, "racing admin edit");
            assert!(cli.is_ok(), "round {i}: neither got `{prefix}`: {cli:?}");
            wins.1 += 1;
        }
    }
    eprintln!("admin won {} / cli won {}", wins.0, wins.1);
}

/// Two admin creates of one free host: MySQL breaks the tie with a
/// deadlock, which must retry and read "already used", not a failed save.
async fn admin_org_creates_racing_for_a_host_one_wins(pool: &Pool) {
    for i in 0..10 {
        let host = format!("{}.race.test", name(pool, &format!("host{i}")));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let create = |slug: String| {
            let (pool, barrier, form) = (pool.clone(), barrier.clone(), org_form(&slug, &host, "", ""));
            tokio::spawn(async move {
                barrier.wait().await;
                send(&pool, Method::POST, "/rustango_orgs", form).await
            })
        };
        let a = create(name(pool, &format!("ra{i}")));
        let b = create(name(pool, &format!("rb{i}")));
        let results = [a.await.unwrap(), b.await.unwrap()];
        let won = results.iter().filter(|(s, _)| s.is_redirection()).count();
        assert_eq!(won, 1, "round {i}: {results:?}");
        let (status, html) = results.iter().find(|(s, _)| !s.is_redirection()).unwrap();
        refused(*status, html, "racing admin create");
    }
}

/// An extra host added inside the Org page is claimed like one added alone.
async fn admin_inline_host_refuses_a_base_host(pool: &Pool) {
    let (host, _, _) = owner(pool).await;
    let slug = name(pool, "inline");
    let id = org(pool, &slug).await;
    let form = format!(
        "{}&rustango_org_hosts-TOTAL_FORMS=1&rustango_org_hosts-INITIAL_FORMS=0\
         &rustango_org_hosts-0-hostname={}&rustango_org_hosts-0-enabled=true",
        org_form(&slug, "", "", ""),
        host.to_uppercase()
    );
    let (status, html) = send(pool, Method::POST, &format!("/rustango_orgs/{id}"), form).await;
    refused(status, &html, "inline extra host");
    let rows: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::org_id.eq(id))
        .fetch(pool)
        .await
        .unwrap();
    assert!(rows.is_empty(), "{rows:?}");
}

async fn resolve(pool: &Pool, host: &str) -> Option<String> {
    let (parts, ()) = Request::builder()
        .uri("/")
        .header(header::HOST, host)
        .body(())
        .unwrap()
        .into_parts();
    let org = RegisteredHostResolver.resolve(&parts, pool).await.unwrap();
    org.map(|o| o.slug)
}

/// A tenant suspended or a host removed in the admin stops resolving at
/// once, not when the process cache expires.
async fn admin_writes_clear_the_org_cache(pool: &Pool) {
    let slug = name(pool, "cache");
    let id = org(pool, &slug).await;
    let host = format!("{slug}.cache.test");
    let form = format!("org_id={id}&hostname={host}&enabled=true");
    let (status, html) = send(pool, Method::POST, "/rustango_org_hosts", form).await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(resolve(pool, &host).await.as_deref(), Some(slug.as_str()));
    let suspend = org_form(&slug, "", "", "").replace("&active=true", "");
    let (status, html) = send(pool, Method::POST, &format!("/rustango_orgs/{id}"), suspend).await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(resolve(pool, &host).await, None, "a suspended tenant still resolved");

    let slug = name(pool, "cache2");
    let id = org(pool, &slug).await;
    let host = format!("{slug}.cache.test");
    let mut row = OrgHost {
        id: rustango::sql::Auto::Unset,
        org_id: id,
        hostname: host.clone(),
        enabled: true,
        created_at: rustango::sql::Auto::Unset,
    };
    row.save_pool(pool).await.unwrap();
    assert_eq!(resolve(pool, &host).await.as_deref(), Some(slug.as_str()));
    let hid = row.id.get().unwrap();
    let uri = format!("/rustango_org_hosts/{hid}/delete");
    let (status, html) = send(pool, Method::POST, &uri, String::new()).await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(resolve(pool, &host).await, None, "a deleted host still resolved");
}

/// A REST delete of an extra host stops it resolving at once.
async fn viewset_writes_clear_the_host_cache(pool: &Pool) {
    let slug = name(pool, "vs");
    let id = org(pool, &slug).await;
    let host = format!("{slug}.cache.test");
    let mut row = OrgHost {
        id: rustango::sql::Auto::Unset,
        org_id: id,
        hostname: host.clone(),
        enabled: true,
        created_at: rustango::sql::Auto::Unset,
    };
    row.save_pool(pool).await.unwrap();
    assert_eq!(resolve(pool, &host).await.as_deref(), Some(slug.as_str()));
    use rustango::core::Model as _;
    let app = rustango::viewset::ViewSet::for_model(OrgHost::SCHEMA, &["id", "hostname"])
        .router_pool("/hosts", pool.clone());
    let uri = format!("/hosts/{}", row.id.get().unwrap());
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(resolve(pool, &host).await, None, "a deleted host still resolved");
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        admin_org_creates_racing_for_a_host_one_wins,
        admin_inline_host_refuses_a_base_host,
        admin_writes_clear_the_org_cache,
        viewset_writes_clear_the_host_cache,
        admin_org_edit_refuses_another_tenants_routing,
        admin_org_create_refuses_a_taken_host,
        admin_host_create_refuses_a_base_host,
        admin_org_edit_keeps_and_normalizes_its_own_routing,
        admin_and_cli_racing_for_a_prefix_one_wins,
    ],
}
