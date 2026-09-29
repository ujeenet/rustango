#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds.
//! The template views' `tenant_router()` copies apply global scopes too (#1746),
//! and audit their writes (#1794).

// `not(postgres)`: the `Tenant` extractor looks up `TenantContext<DefaultDb>`,
// which is Postgres once that feature is on.
#![cfg(all(
    feature = "sqlite",
    feature = "tenancy",
    feature = "template_views",
    not(feature = "postgres")
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use rustango::audit::AuditLog;
use rustango::core::{Filter, Model as _, Op, SqlValue, WhereExpr};
use rustango::extractors::TenantContext;
use rustango::sql::{sqlx, Auto, CounterPool as _, FetcherPool as _, Pool, UpdaterPool as _};
use rustango::template_views::{
    DeleteView, DetailView, ListView, TenantBulkActionPoolFn, UpdateView,
};
use rustango::tenancy::{
    session::SessionSecret, ChainResolver, Org, OrgResolver, TenancyError, TenantPools,
};
use rustango::Model;
use tera::Tera;
use tower::ServiceExt as _;

fn visible_only() -> WhereExpr {
    WhereExpr::Predicate(Filter::new("visible", Op::Eq, SqlValue::Bool(true)))
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "scope1746t_note",
    app = "scope1746t",
    global_scope(name = "visible", apply = visible_only),
    audit(track = "tag")
)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub tag: String,
    pub visible: bool,
}

const SECRET: &[u8] = b"scope1746-tenant-secret-32-bytes!!";
const CSRF: &str = "scope1746-csrf-token-scope1746-csrf-token-x";

#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait::async_trait]
impl OrgResolver for FixedResolver {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &Pool,
    ) -> Result<Option<Org>, TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

/// A file, not `:memory:`: the tenant extractor opens its own pool.
fn db_url(name: &str) -> String {
    let path = std::env::temp_dir().join(format!("rustango_scope1746_{name}.sqlite"));
    let _ = std::fs::remove_file(&path);
    format!("sqlite://{}?mode=rwc", path.display())
}

/// A tenant bulk action that writes unscoped, so only the PK narrowing
/// keeps a hidden row out of it.
fn retag_action() -> TenantBulkActionPoolFn {
    Arc::new(|pool, pks| {
        Box::pin(async move {
            Note::objects()
                .without_global_scopes()
                .filter("id__in", SqlValue::List(pks.to_vec()))
                .update()
                .set("tag", "m")
                .execute_pool(pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    })
}

/// The tenant app, its seeding pool, and the `(visible, hidden)` PKs.
async fn app(name: &str) -> (Router, Pool, Vec<i64>, Vec<i64>) {
    let url = db_url(name);
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect(&url).await.expect("tenant db"));
    rustango::testkit::create_tables_for::<Note>(&pool)
        .await
        .expect("notes table");
    rustango::audit::ensure_table_pool(&pool)
        .await
        .expect("audit table");
    let (mut shown, mut hidden) = (Vec::new(), Vec::new());
    for (tag, visible) in [("a", true), ("h", false), ("b", true), ("h", false)] {
        let mut row = Note {
            id: Auto::default(),
            tag: tag.into(),
            visible,
        };
        row.insert_pool(&pool).await.expect("seed row");
        let pk = *row.id.get().expect("pk");
        if visible {
            shown.push(pk);
        } else {
            hidden.push(pk);
        }
    }

    let org = Org {
        slug: "acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(url),
        ..rustango::testkit::org()
    };
    let registry = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry");
    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: Arc::new(TenantPools::new(registry)),
        resolver: ChainResolver::new().push(FixedResolver(org)),
        session_secret: SessionSecret::from_bytes(SECRET.to_vec()),
        operator_secret: SessionSecret::from_bytes(SECRET.to_vec()),
    });

    let mut t = Tera::default();
    t.add_raw_templates(vec![
        (
            "list.html",
            "rows={{ object_list | length }} total={{ total }}",
        ),
        ("detail.html", "tag={{ object.tag }}"),
        ("form.html", "form"),
        ("confirm.html", "confirm"),
        ("bulk.html", "objects={{ objects | length }}"),
    ])
    .unwrap();
    let t = Arc::new(t);

    let router = ListView::for_model(Note::SCHEMA)
        .template("list.html")
        .bulk_actions(true)
        .tenant_action_pool("retag", "Retag", retag_action())
        .with_delete_confirmation(true)
        .with_delete_confirmation_template("bulk.html")
        .tenant_router("/notes", t.clone())
        .merge(
            DetailView::for_model(Note::SCHEMA)
                .template("detail.html")
                .tenant_router("/notes", t.clone()),
        )
        .merge(
            UpdateView::for_model(Note::SCHEMA)
                .template("form.html")
                .success_url("/notes")
                .tenant_router("/notes", t.clone()),
        )
        .merge(
            DeleteView::for_model(Note::SCHEMA)
                .template("confirm.html")
                .success_url("/notes")
                .tenant_router("/notes", t),
        )
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));
    (router, pool, shown, hidden)
}

async fn send(app: &Router, method: Method, uri: &str, form: Option<&str>) -> (StatusCode, String) {
    let body = form.map_or_else(String::new, |f| format!("_csrf={CSRF}&{f}"));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
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

async fn audit_rows(pool: &Pool, operation: &str) -> i64 {
    AuditLog::objects()
        .filter("entity_table", Note::SCHEMA.table)
        .filter("operation", operation)
        .count(pool)
        .await
        .expect("count audit rows")
}

async fn row(pool: &Pool, pk: i64) -> Option<Note> {
    Note::objects()
        .without_global_scopes()
        .fetch(pool)
        .await
        .expect("unscoped fetch")
        .into_iter()
        .find(|r| *r.id.get().unwrap() == pk)
}

#[tokio::test]
async fn tenant_list_hides_scoped_rows() {
    let (app, _, _, _) = app("list").await;
    let (status, body) = send(&app, Method::GET, "/notes", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "rows=2 total=2");
}

#[tokio::test]
async fn tenant_pk_views_404_on_hidden_rows() {
    let (app, pool, shown, hidden) = app("pk").await;
    let (h, s) = (hidden[0], shown[0]);
    for uri in [
        format!("/notes/{s}"),
        format!("/notes/{s}/edit"),
        format!("/notes/{s}/delete"),
    ] {
        let (status, _) = send(&app, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::OK, "control: GET {uri}");
    }
    for uri in [
        format!("/notes/{h}"),
        format!("/notes/{h}/edit"),
        format!("/notes/{h}/delete"),
    ] {
        let (status, _) = send(&app, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}");
    }
    let edit = "tag=x&visible=true";
    let (status, _) = send(&app, Method::POST, &format!("/notes/{h}/edit"), Some(edit)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "UpdateView POST");
    let (status, _) = send(&app, Method::POST, &format!("/notes/{h}/delete"), Some("")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "DeleteView POST");
    let hidden_row = row(&pool, h).await.expect("hidden row kept");
    assert_eq!(hidden_row.tag, "h", "hidden row not updated");

    let (status, _) = send(&app, Method::POST, &format!("/notes/{s}/edit"), Some(edit)).await;
    assert!(
        status.is_redirection(),
        "UpdateView POST on a visible row: {status}"
    );
    assert_eq!(row(&pool, s).await.unwrap().tag, "x");
    assert_eq!(audit_rows(&pool, "update").await, 1, "UpdateView audited");
    let (status, _) = send(&app, Method::POST, &format!("/notes/{s}/delete"), Some("")).await;
    assert!(
        status.is_redirection(),
        "DeleteView POST on a visible row: {status}"
    );
    assert!(row(&pool, s).await.is_none(), "visible row deleted");
    assert_eq!(audit_rows(&pool, "delete").await, 1, "DeleteView audited");
}

#[tokio::test]
async fn tenant_bulk_actions_skip_hidden_rows() {
    let (app, pool, shown, hidden) = app("bulk").await;
    let pick = |action: &str| {
        format!(
            "action={action}&_selected_action={}&_selected_action={}",
            hidden[0], shown[0]
        )
    };
    let (status, body) = send(&app, Method::POST, "/notes", Some(&pick("retag"))).await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(row(&pool, shown[0]).await.unwrap().tag, "m", "visible row");
    assert_eq!(row(&pool, hidden[0]).await.unwrap().tag, "h", "hidden row");

    let del = pick("delete_selected");
    let (_, body) = send(&app, Method::POST, "/notes", Some(&del)).await;
    assert_eq!(body, "objects=1", "confirm page lists only the visible row");
    let confirmed = format!("{del}&confirmed=true");
    let (status, _) = send(&app, Method::POST, "/notes", Some(&confirmed)).await;
    assert!(status.is_redirection(), "{status}");
    assert!(row(&pool, hidden[0]).await.is_some(), "hidden row kept");
    assert!(row(&pool, shown[0]).await.is_none(), "visible row deleted");
}
