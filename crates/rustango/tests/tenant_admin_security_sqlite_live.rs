#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! Tenant admin permission gaps: the audit feed (#1858). SQLite, no service.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::audit::{AuditOp, AuditSource, PendingEntry};
use rustango::sql::sqlx;
use rustango::tenancy::permissions::set_user_perm_pool;
use rustango::tenancy::tenant_console::{
    encode, PasswordFingerprint, SessionSecret, TenantSessionPayload, COOKIE_NAME,
};
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, ChainResolver, Org, SubdomainResolver,
    TenantPools, User,
};
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sec_note", display = "title", admin(list_display = "title"))]
#[allow(dead_code)]
pub struct SecNote {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub owner_id: i64,
}

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Env {
    admin: axum::Router,
    tenant: rustango::sql::Pool,
    secret: SessionSecret,
    slug: String,
    host: String,
    _pools: Arc<TenantPools<sqlx::Sqlite>>,
    _dir: tempfile::TempDir,
}

/// A migrated registry, one database-mode tenant with every framework
/// table plus `sec_note`, and the tenant admin on legacy routes.
async fn boot() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg_url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let tenant_url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(
        sqlx::SqlitePool::connect(&reg_url).await.expect("registry"),
    ));
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    let mut out = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &reg_url,
        &migrations,
        vec!["migrate-registry".to_owned()],
        &mut out,
    )
    .await
    .expect("migrate-registry");
    let registry = pools.registry_pool();

    let slug = unique("t");
    let host = format!("{slug}.app.test");
    let mut org = Org {
        slug: slug.clone(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(tenant_url.clone()),
        host_pattern: Some(host.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.expect("seed org");

    let tenant = rustango::sql::Pool::connect(&tenant_url)
        .await
        .expect("tenant");
    rustango::testkit::migrate_framework(&tenant)
        .await
        .expect("framework tables");
    rustango::testkit::create_tables_for::<SecNote>(&tenant)
        .await
        .expect("sec_note table");

    let secret = SessionSecret::from_bytes(b"tenant-admin-security-secret-32b".to_vec());
    let admin = TenantAdminBuilder::new(
        pools.clone(),
        reg_url,
        ChainResolver::new().push(SubdomainResolver::new("app.test")),
    )
    .routes(RouteConfig::legacy())
    .with_session(secret.clone())
    .build();
    Env {
        admin,
        tenant,
        secret,
        slug,
        host,
        _pools: pools,
        _dir: dir,
    }
}

impl Env {
    /// A signed-in tenant user holding exactly `perms`. Returns the cookie.
    async fn login(&self, is_superuser: bool, perms: &[&str]) -> String {
        let mut user = User {
            username: unique("u"),
            is_superuser,
            password_hash: rustango::tenancy::password::hash("pw-123456789").unwrap(),
            ..rustango::testkit::user()
        };
        user.insert_pool(&self.tenant).await.expect("seed user");
        let uid = user.id.get().copied().unwrap();
        for p in perms {
            set_user_perm_pool(uid, p, true, &self.tenant)
                .await
                .expect("grant");
        }
        let login = TenantSessionPayload::new(
            uid,
            &self.slug,
            3600,
            PasswordFingerprint::of(&self.secret, &user.password_hash),
        );
        format!(
            "{COOKIE_NAME}={}; rustango_csrf=t",
            encode(&self.secret, &login)
        )
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, String) {
        let res = self.admin.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get(&self, uri: &str, cookie: &str) -> (StatusCode, String) {
        self.send(
            Request::builder()
                .uri(uri)
                .header(header::HOST, &self.host)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, uri: &str, cookie: &str, body: &str) -> (StatusCode, String) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::HOST, &self.host)
                .header(header::COOKIE, cookie)
                .header("x-csrf-token", "t")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
    }

    async fn audit(&self, table: &'static str, pk: &str) {
        rustango::audit::emit_one_pool(
            &self.tenant,
            &PendingEntry {
                entity_table: table,
                entity_pk: pk.to_owned(),
                operation: AuditOp::Update,
                source: AuditSource::System,
                changes: serde_json::json!({ "marker": format!("{table}-{pk}") }),
            },
        )
        .await
        .expect("emit audit");
    }

    async fn audit_rows(&self) -> i64 {
        rustango::audit::count(&self.tenant, &rustango::audit::AuditFilter::default())
            .await
            .expect("count audit")
    }
}

/// #1858: a user with no `audit.view` gets 403 on the feed.
#[tokio::test]
async fn the_audit_feed_needs_audit_view() {
    let env = boot().await;
    env.audit("rustango_users", "1").await;
    let cookie = env.login(false, &["sec_note.view"]).await;
    let (status, body) = env.get("/__admin/__audit", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("rustango_users-1"), "{body}");
    let (status, body) = env.get("/__admin/", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("rustango_users/1"), "recent actions: {body}");
}

/// #1858: `audit.view` shows only rows of tables the user may view.
#[tokio::test]
async fn the_audit_feed_shows_only_viewable_tables() {
    let env = boot().await;
    env.audit("rustango_users", "1").await;
    env.audit("sec_note", "7").await;
    let cookie = env.login(false, &["audit.view", "sec_note.view"]).await;
    let (status, body) = env.get("/__admin/__audit", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("sec_note-7"), "{body}");
    assert!(!body.contains("rustango_users-1"), "{body}");
    assert!(!body.contains(">rustango_users<"), "facet rail: {body}");
}

/// #1858: cleanup without `audit.delete` is refused and deletes nothing.
#[tokio::test]
async fn audit_cleanup_needs_audit_delete() {
    let env = boot().await;
    env.audit("sec_note", "1").await;
    let cookie = env.login(false, &["audit.view", "sec_note.view"]).await;
    let (status, _) = env
        .post(
            "/__admin/__audit/cleanup",
            &cookie,
            "mode=older_than&days=0",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(env.audit_rows().await, 1, "the trail must survive");
}

/// #1858: a string PK cannot break out of the feed's `href`.
#[tokio::test]
async fn an_audit_pk_cannot_inject_markup() {
    let env = boot().await;
    env.audit("sec_note", r#"x" onmouseover="alert(1)"#).await;
    let cookie = env.login(true, &[]).await;
    let (status, body) = env.get("/__admin/__audit", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(r#"" onmouseover=""#), "{body}");
}
