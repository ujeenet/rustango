#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! Tenant admin permission gaps (#1858, #1859, #1860, #1863). SQLite, no service.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::audit::{AuditOp, AuditSource, PendingEntry};
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::{sqlx, FetcherPool as _};
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

/// A table scoped by `register_admin_queryset!` to the signed-in user's rows.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "sec_owned",
    display = "title",
    admin(
        list_display = "title",
        list_filter = "owner_id",
        search_fields = "title",
        actions = "delete_selected"
    )
)]
#[allow(dead_code)]
pub struct SecOwned {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub owner_id: i64,
}

fn own_rows(parts: &axum::http::request::Parts) -> Vec<Filter> {
    let uid = parts
        .extensions
        .get::<rustango::admin::AdminSession>()
        .map_or(-1, |s| s.user_id);
    vec![Filter::new("owner_id", Op::Eq, SqlValue::I64(uid))]
}
rustango::register_admin_queryset!("sec_owned", own_rows);

/// Two hidden fields (`editable = false`) and one left out of `fieldsets`.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "sec_form",
    display = "title",
    admin(list_display = "title", fieldsets = "Main: title")
)]
#[allow(dead_code)]
pub struct SecForm {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(editable = false, default = "0")]
    pub owner_id: i64,
    #[rustango(editable = false, default = "false")]
    pub is_verified: bool,
    #[rustango(max_length = 200)]
    pub note: Option<String>,
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
    rustango::testkit::create_tables_for::<SecOwned>(&tenant)
        .await
        .expect("sec_owned table");
    rustango::testkit::create_tables_for::<SecForm>(&tenant)
        .await
        .expect("sec_form table");
    rustango::i18n::db::ensure_table_pool(&tenant)
        .await
        .expect("translations table");

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
        self.login_as(is_superuser, perms).await.0
    }

    /// As [`Self::login`], plus the user's id.
    async fn login_as(&self, is_superuser: bool, perms: &[&str]) -> (String, i64) {
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
        let cookie = format!(
            "{COOKIE_NAME}={}; rustango_csrf=t",
            encode(&self.secret, &login)
        );
        (cookie, uid)
    }

    async fn owned(&self, title: &str, owner_id: i64) -> i64 {
        let mut row = SecOwned {
            id: rustango::Auto::default(),
            title: title.to_owned(),
            owner_id,
        };
        row.insert_pool(&self.tenant).await.expect("seed sec_owned");
        row.id.get().copied().unwrap()
    }

    async fn owned_titles(&self) -> Vec<String> {
        let mut t: Vec<String> = SecOwned::objects()
            .fetch(&self.tenant)
            .await
            .expect("fetch sec_owned")
            .into_iter()
            .map(|r| r.title)
            .collect();
        t.sort();
        t
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

/// #1863: a non-superuser in the tenant admin cannot write translations.
#[tokio::test]
async fn translation_edits_need_a_superuser_in_the_tenant_admin() {
    let env = boot().await;
    let editor = "/__admin/rustango_translations/editor";
    let cookie = env.login(false, &["rustango_translations.view"]).await;
    let (status, body) = env.post(editor, &cookie, "tr:en:greeting=pwned").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let rows = rustango::i18n::db::all_pool(&env.tenant).await.unwrap();
    assert!(rows.is_empty(), "nothing written: {rows:?}");

    let root = env.login(true, &[]).await;
    let (status, body) = env.post(editor, &root, "tr:en:greeting=Hello").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let rows = rustango::i18n::db::all_pool(&env.tenant).await.unwrap();
    assert_eq!(rows.len(), 1, "a superuser still saves");
}

/// A user holding `sec_owned` perms: `(env, cookie, own pk, other pk, other owner)`.
async fn owned_env() -> (Env, String, i64, i64, i64) {
    let env = boot().await;
    let perms = ["sec_owned.view", "sec_owned.change", "sec_owned.delete"];
    let (cookie, uid) = env.login_as(false, &perms).await;
    let mine = env.owned("mine-note", uid).await;
    let theirs = env.owned("their-secret", uid + 1000).await;
    (env, cookie, mine, theirs, uid + 1000)
}

/// #1859: a row the queryset hook hides is a 404 on every by-pk route.
#[tokio::test]
async fn the_queryset_hook_scopes_by_pk_routes() {
    let (env, cookie, mine, theirs, _) = owned_env().await;
    let base = "/__admin/sec_owned";
    assert_eq!(
        env.get(&format!("{base}/{mine}"), &cookie).await.0,
        StatusCode::OK
    );
    for uri in [format!("{base}/{theirs}"), format!("{base}/{theirs}/edit")] {
        let (status, body) = env.get(&uri, &cookie).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert!(!body.contains("their-secret"), "{uri}: {body}");
    }
    let (status, _) = env
        .post(
            &format!("{base}/{theirs}"),
            &cookie,
            "title=hacked&owner_id=1",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "update");
    let (status, _) = env
        .post(&format!("{base}/{theirs}/delete"), &cookie, "")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "delete");
    assert_eq!(env.owned_titles().await, ["mine-note", "their-secret"]);
}

/// #1859: bulk actions skip rows outside the hook's scope.
#[tokio::test]
async fn the_queryset_hook_scopes_actions() {
    let (env, cookie, mine, theirs, _) = owned_env().await;
    let form = format!("action=delete_selected&_selected={mine}&_selected={theirs}");
    env.post("/__admin/sec_owned/__action", &cookie, &form)
        .await;
    assert_eq!(env.owned_titles().await, ["their-secret"]);
}

/// #1859: autocomplete sees only scoped rows.
#[tokio::test]
async fn the_queryset_hook_scopes_autocomplete() {
    let (env, cookie, _, _, _) = owned_env().await;
    let (_, body) = env
        .get("/__admin/sec_owned/__autocomplete?q=", &cookie)
        .await;
    assert!(body.contains("mine-note"), "{body}");
    assert!(!body.contains("their-secret"), "autocomplete: {body}");
}

/// #1859: facet counts see only scoped rows.
#[tokio::test]
async fn the_queryset_hook_scopes_facets() {
    let (env, cookie, _, _, their_owner) = owned_env().await;
    let (status, body) = env.get("/__admin/sec_owned", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body.contains(&format!("owner_id={their_owner}")),
        "facet: {body}"
    );
}

async fn sec_forms(env: &Env) -> Vec<SecForm> {
    SecForm::objects()
        .order_by(&[("id", false)])
        .fetch(&env.tenant)
        .await
        .expect("fetch sec_form")
}

/// #1860: an edit POST cannot write, or clear, fields the form hides.
#[tokio::test]
async fn an_edit_writes_only_rendered_fields() {
    let env = boot().await;
    let cookie = env.login(true, &[]).await;
    let mut row = SecForm {
        id: rustango::Auto::default(),
        title: "old".into(),
        owner_id: 5,
        is_verified: true,
        note: Some("keep".into()),
    };
    row.insert_pool(&env.tenant).await.expect("seed sec_form");
    let pk = row.id.get().copied().unwrap();
    let (status, body) = env
        .post(
            &format!("/__admin/sec_form/{pk}"),
            &cookie,
            "title=new&owner_id=99&note=pwned",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let got = &sec_forms(&env).await[0];
    assert_eq!(got.title, "new");
    assert_eq!(got.owner_id, 5, "editable = false must not be written");
    assert!(got.is_verified, "a hidden bool must not reset to false");
    assert_eq!(got.note.as_deref(), Some("keep"), "outside fieldsets");
}

/// #1860: a create POST cannot set fields the form hides.
#[tokio::test]
async fn a_create_writes_only_rendered_fields() {
    let env = boot().await;
    let cookie = env.login(true, &[]).await;
    let (status, body) = env
        .post(
            "/__admin/sec_form",
            &cookie,
            "title=c&owner_id=99&is_verified=on&note=pwned",
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let got = &sec_forms(&env).await[0];
    assert_eq!(got.title, "c");
    assert_eq!(got.owner_id, 0);
    assert!(!got.is_verified);
    assert_eq!(got.note, None);
}
