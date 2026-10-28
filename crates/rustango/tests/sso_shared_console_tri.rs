//! The operator console's shared SSO provider forms, on every backend.

#![cfg(all(feature = "tenancy", feature = "admin-sso", feature = "testkit"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::tenancy::{Operator, TenantPoolInvalidator, TenantPools};
use rustango::testkit::sso_check::SharedSsoProvider;
use rustango::tri_dialect_test;
use tower::ServiceExt;

const PASSWORD: &str = "operator-password-sso";

async fn setup(pool: &Pool) {
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-shared-console-tri-key");
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

struct Console {
    app: Router,
    pool: Pool,
    cookie: String,
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

async fn console(pool: &Pool) -> Console {
    let app = rustango::tenancy::operator_console::router_with_pools(
        pool.clone(),
        invalidator(pool),
        rustango::tenancy::operator_console::SessionSecret::from_bytes(vec![5u8; 32]),
    );
    let mut op = Operator {
        id: Auto::default(),
        username: unique("op"),
        password_hash: rustango::tenancy::password::hash(PASSWORD).unwrap(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(pool).await.expect("seed operator");
    let mut c = Console {
        app,
        pool: pool.clone(),
        cookie: String::new(),
    };
    let login = c
        .send(
            "POST",
            "/login",
            format!("username={}&password={PASSWORD}", op.username),
        )
        .await;
    assert!(login.status().is_redirection(), "operator login");
    c.cookie = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter(|v| !v.starts_with("rustango_csrf="))
        .collect::<Vec<_>>()
        .join("; ");
    c
}

impl Console {
    async fn send(&self, method: &str, uri: &str, body: String) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::HOST, "console.test")
                    .header(header::ACCEPT, "text/html")
                    .header(header::COOKIE, format!("rustango_csrf=t; {}", self.cookie))
                    .header("x-csrf-token", "t")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn html(&self, method: &str, uri: &str, body: String) -> (StatusCode, String) {
        let resp = self.send(method, uri, body).await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn row(&self, slug: &str) -> SharedSsoProvider {
        SharedSsoProvider::objects()
            .filter("slug", slug.to_owned())
            .fetch(&self.pool)
            .await
            .unwrap()
            .remove(0)
    }

    /// Create a `google` provider with secret `first-secret`; its id.
    async fn create(&self, slug: &str) -> i64 {
        let form = format!(
            "slug={slug}&label=Corp&kind=google&client_id=cid&client_secret=first-secret&enabled=on"
        );
        let resp = self.send("POST", "/sso-shared", form).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        self.row(slug).await.id.get().copied().unwrap()
    }
}

/// The edit form updates the row in place; a blank secret keeps the saved one.
async fn an_edit_keeps_the_id_and_a_blank_secret(pool: &Pool) {
    let c = console(pool).await;
    let slug = unique("corp");
    let id = c.create(&slug).await;
    let (status, html) = c
        .html("GET", &format!("/sso-shared/{id}/edit"), String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("value=\"Corp\"") && !html.contains("first-secret"),
        "{html}"
    );

    let edit = |label: &str, secret: &str| {
        format!(
            "label={label}&kind=oidc&issuer_url=https%3A%2F%2Fidp.example&client_id=cid2\
             &client_secret={secret}&scopes=openid&sort_order=7&allow_email_link=on"
        )
    };
    let path = format!("/sso-shared/{id}/edit");
    let resp = c.send("POST", &path, edit("Renamed", "")).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let row = c.row(&slug).await;
    assert_eq!(row.id.get().copied(), Some(id));
    assert_eq!(row.label, "Renamed");
    assert_eq!(row.kind, "oidc");
    assert_eq!(row.issuer_url.as_deref(), Some("https://idp.example"));
    assert_eq!(row.client_id, "cid2");
    assert_eq!(row.scopes.as_deref(), Some("openid"));
    assert_eq!(row.sort_order, 7);
    assert!(!row.enabled && row.allow_email_link);
    assert_eq!(row.client_secret.as_str(), "first-secret");

    let resp = c
        .send("POST", &path, edit("Renamed", "second-secret"))
        .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let row = c.row(&slug).await;
    assert_eq!(row.id.get().copied(), Some(id));
    assert_eq!(row.client_secret.as_str(), "second-secret");

    let audit = rustango::audit::fetch_for_entity_pool(
        pool,
        "rustango_shared_sso_providers",
        &id.to_string(),
    )
    .await
    .unwrap();
    let updates: Vec<_> = audit
        .iter()
        .filter(|e| e.source.ends_with(":sso_shared_update"))
        .map(|e| e.changes.to_string())
        .filter(|c| c.contains(&slug))
        .collect();
    assert_eq!(updates.len(), 2, "{audit:?}");
    assert!(
        updates
            .iter()
            .all(|c| !c.contains("secret\"") && !c.contains("-secret")),
        "{updates:?}"
    );
}

/// A refused edit re-renders the form with what was typed, never the secret.
async fn a_bad_edit_rerenders_with_the_typed_values(pool: &Pool) {
    let c = console(pool).await;
    let slug = unique("corp");
    let id = c.create(&slug).await;
    let form =
        "label=Typed+Label&kind=oidc&issuer_url=&client_id=typed-cid&client_secret=typed-secret";
    let (status, html) = c
        .html("POST", &format!("/sso-shared/{id}/edit"), form.into())
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(html.contains("alert-error"), "{html}");
    assert!(
        html.contains("value=\"Typed Label\"") && html.contains("value=\"typed-cid\""),
        "{html}"
    );
    assert!(!html.contains("typed-secret"), "{html}");
    let row = c.row(&slug).await;
    assert_eq!((row.label.as_str(), row.kind.as_str()), ("Corp", "google"));
    assert_eq!(row.client_secret.as_str(), "first-secret");
}

/// A refused create re-renders the add form with what was typed, never the secret.
async fn a_bad_create_rerenders_with_the_typed_values(pool: &Pool) {
    let c = console(pool).await;
    let slug = unique("corp");
    c.create(&slug).await;
    for (form, message) in [
        (
            "slug=a%2Fb&label=Typed+Label&kind=google&client_id=c&client_secret=typed-secret"
                .to_owned(),
            "slug must be",
        ),
        (
            format!(
                "slug={slug}&label=Typed+Label&kind=google&client_id=c&client_secret=typed-secret"
            ),
            "already exists",
        ),
        (
            "slug=ok-slug&label=Typed+Label&kind=oidc&client_id=c&client_secret=typed-secret"
                .to_owned(),
            "issuer",
        ),
    ] {
        let (status, html) = c.html("POST", "/sso-shared", form.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{form}");
        assert!(
            html.contains("alert-error") && html.contains(message),
            "{form}: {html}"
        );
        assert!(html.contains("value=\"Typed Label\""), "{form}: {html}");
        assert!(!html.contains("typed-secret"), "{form}: {html}");
    }
}

/// The error-pages layer, outermost as `server::Builder` mounts it, keeps
/// the console's own form page for a refused create.
async fn the_error_pages_layer_keeps_the_console_form(pool: &Pool) {
    #[cfg(feature = "template_views")]
    {
        use rustango::errors::{ErrorPagesLayer, ErrorPagesRouterExt as _};
        let mut c = console(pool).await;
        c.app = c.app.clone().error_pages(ErrorPagesLayer::new());
        let form = "slug=ok-slug&label=Typed+Label&kind=oidc&client_id=c&client_secret=s";
        let (status, html) = c.html("POST", "/sso-shared", form.to_owned()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            html.contains("alert-error") && html.contains("issuer"),
            "{html}"
        );
        assert!(html.contains("value=\"Typed Label\""), "{html}");
    }
    #[cfg(not(feature = "template_views"))]
    let _ = pool;
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        an_edit_keeps_the_id_and_a_blank_secret,
        a_bad_edit_rerenders_with_the_typed_values,
        a_bad_create_rerenders_with_the_typed_values,
        the_error_pages_layer_keeps_the_console_form,
    ],
}
