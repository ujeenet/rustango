//! Schema-driven writes on audited models write audit rows: ViewSet,
//! template views and `soft_delete` (#1794).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin",
    feature = "template_views"
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::audit::{self, AuditLog};
use rustango::core::{Model as _, SqlValue};
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1794_doc",
    app = "audit1794",
    audit(track = "title, deleted_at")
)]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

const DOC: &str = "audit1794_doc";
const CSRF: &str = "audit-view-writes-csrf-token-audit-view-wr";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    AuditLog::delete_where("entity_table", DOC, pool)
        .await
        .expect("clear audit rows");
}

async fn ops(pool: &Pool, operation: &str) -> i64 {
    AuditLog::objects()
        .filter("entity_table", DOC)
        .filter("operation", operation)
        .count(pool)
        .await
        .expect("count audit rows")
}

/// Two docs; returns their PKs.
async fn seed(pool: &Pool) -> Vec<i64> {
    let mut pks = Vec::new();
    for title in ["a", "b"] {
        let mut doc = Doc {
            id: Auto::default(),
            title: title.into(),
            deleted_at: None,
        };
        doc.insert_pool(pool).await.expect("insert");
        pks.push(*doc.id.get().expect("pk"));
    }
    pks
}

async fn send(
    app: axum::Router,
    method: Method,
    uri: &str,
    body: String,
    form: bool,
) -> StatusCode {
    let mut req = Request::builder().method(method).uri(uri);
    req = if form {
        req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
    } else {
        req.header(header::CONTENT_TYPE, "application/json")
    };
    let body = if form {
        format!("_csrf={CSRF}&{body}")
    } else {
        body
    };
    app.oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

/// POST a JSON body; the status and the parsed response body.
async fn post_json(app: axum::Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// The `entity_pk` of every `create` row for `table`.
async fn created_pks(pool: &Pool, table: &str) -> Vec<String> {
    AuditLog::objects()
        .filter("entity_table", table)
        .filter("operation", "create")
        .fetch(pool)
        .await
        .expect("fetch audit rows")
        .into_iter()
        .map(|r| r.entity_pk)
        .collect()
}

async fn viewset_update_and_delete_are_audited(pool: &Pool) {
    let pks = seed(pool).await;
    let app =
        || rustango::viewset::ViewSet::for_model(Doc::SCHEMA).router_pool("/docs", pool.clone());
    let uri = format!("/docs/{}", pks[0]);
    let status = send(app(), Method::PATCH, &uri, r#"{"title":"x"}"#.into(), false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ops(pool, "update").await, 1);
    let status = send(app(), Method::DELETE, &uri, String::new(), false).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // `Doc` is soft-delete, so the ViewSet stamps it (#1998).
    assert_eq!(ops(pool, "soft_delete").await, 1);
    assert_eq!(ops(pool, "delete").await, 0);
}

/// Single and bulk create each write one `create` row (#1816).
async fn viewset_create_is_audited(pool: &Pool) {
    let app =
        || rustango::viewset::ViewSet::for_model(Doc::SCHEMA).router_pool("/docs", pool.clone());
    let (status, body) = post_json(app(), "/docs", r#"{"title":"n"}"#).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created_pks(pool, DOC).await, [body["id"].to_string()]);
    let many = r#"[{"title":"x"},{"title":"y"}]"#.to_owned();
    let status = send(app(), Method::POST, "/docs", many, false).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ops(pool, "create").await, 3);
}

async fn template_views_writes_are_audited(pool: &Pool) {
    use rustango::template_views::{DeleteView, ListView, UpdateView};
    let pks = seed(pool).await;
    let tera = Arc::new(tera::Tera::default());

    let app = UpdateView::for_model(Doc::SCHEMA).router("/docs", tera.clone(), pool.clone());
    let uri = format!("/docs/{}/edit", pks[0]);
    let status = send(app, Method::POST, &uri, "title=x".into(), true).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(ops(pool, "update").await, 1);

    let app = DeleteView::for_model(Doc::SCHEMA).router("/docs", tera.clone(), pool.clone());
    let uri = format!("/docs/{}/delete", pks[0]);
    let status = send(app, Method::POST, &uri, String::new(), true).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(ops(pool, "delete").await, 1);

    let app =
        ListView::for_model(Doc::SCHEMA)
            .bulk_actions(true)
            .router("/docs", tera, pool.clone());
    let form = format!("action=delete_selected&_selected_action={}", pks[1]);
    let status = send(app, Method::POST, "/docs", form, true).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(ops(pool, "delete").await, 2);
}

/// CreateView, with and without a `{pk}` success URL, and `ModelForm`
/// writes each audit with the real PK (#1821).
async fn template_create_and_model_form_are_audited(pool: &Pool) {
    use rustango::template_views::CreateView;
    let tera = Arc::new(tera::Tera::default());
    for url in ["/docs", "/docs/{pk}"] {
        let app = CreateView::for_model(Doc::SCHEMA).success_url(url).router(
            "/docs",
            tera.clone(),
            pool.clone(),
        );
        let status = send(app, Method::POST, "/docs/new", "title=c".into(), true).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }
    let docs = Doc::objects().fetch(pool).await.expect("fetch");
    let mut pks: Vec<String> = docs
        .iter()
        .map(|d| d.id.get().unwrap().to_string())
        .collect();
    let mut audited = created_pks(pool, DOC).await;
    pks.sort();
    audited.sort();
    assert_eq!(audited, pks);

    let data = |t: &str| [("title".to_owned(), t.to_owned())].into_iter().collect();
    let form = rustango::forms::ModelForm::new(Doc::SCHEMA, data("f"));
    let pk = form.save(pool).await.expect("form insert");
    assert!(created_pks(pool, DOC)
        .await
        .contains(&pk.to_display_string()));
    let form = rustango::forms::ModelForm::for_update(Doc::SCHEMA, data("g"), pk);
    form.save(pool).await.expect("form update");
    assert_eq!(ops(pool, "update").await, 1);
}

async fn soft_delete_restore_and_purge_are_audited(pool: &Pool) {
    use rustango::soft_delete;
    let pks = seed(pool).await;
    let pk = || SqlValue::I64(pks[0]);
    assert_eq!(
        soft_delete::soft_delete(pool, Doc::SCHEMA, "id", pk())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        soft_delete::restore(pool, Doc::SCHEMA, "id", pk())
            .await
            .unwrap(),
        1
    );
    let active = SqlValue::I64(pks[1]);
    let restored = soft_delete::restore(pool, Doc::SCHEMA, "id", active);
    assert_eq!(restored.await.unwrap(), 0, "an active row is not restored");
    assert_eq!(ops(pool, "soft_delete").await, 1);
    assert_eq!(ops(pool, "restore").await, 1);
    assert_eq!(ops(pool, "update").await, 0);
    assert_eq!(
        soft_delete::purge(pool, Doc::SCHEMA, "id", pk())
            .await
            .unwrap(),
        1
    );
    assert_eq!(ops(pool, "delete").await, 1);
}

/// ViewSet create on SQLite: generated UUID PKs and audit failures (#1816).
#[cfg(feature = "sqlite")]
mod sqlite_create {
    use super::*;

    /// A DB-generated UUID PK.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "audit1816_tok", app = "audit1794", audit(track = "name"))]
    #[allow(dead_code)]
    pub struct Tok {
        #[rustango(auto_uuid, default = "(randomblob(16))")]
        pub id: Auto<uuid::Uuid>,
        #[rustango(max_length = 32)]
        pub name: String,
    }

    #[tokio::test]
    async fn viewset_create_audits_a_db_generated_uuid_pk() {
        let pool = rustango::testkit::matrix::sqlite_file_pool().await;
        rustango::testkit::matrix::fresh_table::<Tok>(&pool).await;
        audit::ensure_table_pool(&pool).await.expect("audit table");
        let app = ViewSet::for_model(Tok::SCHEMA).router_pool("/toks", pool.clone());
        let (status, body) = post_json(app, "/toks", r#"{"name":"n"}"#).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let row = Tok::objects().fetch(&pool).await.unwrap().remove(0);
        let id = row.id.get().expect("pk").to_string();
        assert_eq!(body["id"], id.as_str());
        assert_eq!(created_pks(&pool, "audit1816_tok").await, [id]);
    }

    /// A failed audit write is a server fault that rolls the create back.
    #[tokio::test]
    async fn viewset_create_audit_failure_is_a_500() {
        let pool = rustango::testkit::matrix::sqlite_file_pool().await;
        rustango::testkit::matrix::fresh_table::<Doc>(&pool).await;
        let app = || ViewSet::for_model(Doc::SCHEMA).router_pool("/docs", pool.clone());
        for body in [r#"{"title":"n"}"#, r#"[{"title":"n"}]"#] {
            let (status, _) = post_json(app(), "/docs", body).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        }
        assert!(Doc::objects().fetch(&pool).await.unwrap().is_empty());
    }

    use rustango::viewset::ViewSet;
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        viewset_update_and_delete_are_audited,
        viewset_create_is_audited,
        template_views_writes_are_audited,
        template_create_and_model_form_are_audited,
        soft_delete_restore_and_purge_are_audited,
    ],
}

/// `bulk_actions` needs `tenancy`.
#[cfg(feature = "tenancy")]
mod bulk {
    use super::*;
    use rustango::bulk_actions::{
        BulkAction as _, BulkDeleteAction, BulkRestoreAction, BulkSoftDeleteAction, PkSet,
    };

    /// Soft delete skips deleted rows and restore skips active ones, so
    /// each writes one audit row per changed row.
    async fn bulk_actions_are_audited(pool: &Pool) {
        let pks = seed(pool).await;
        let one = PkSet::new(Doc::SCHEMA, pks[..1].iter().copied()).unwrap();
        let all = PkSet::new(Doc::SCHEMA, pks.iter().copied()).unwrap();
        let column = "deleted_at";
        let soft = BulkSoftDeleteAction { column };
        assert_eq!(soft.run(&one, pool).await.unwrap().affected, 1);
        assert_eq!(soft.run(&all, pool).await.unwrap().affected, 1);
        assert_eq!(ops(pool, "soft_delete").await, 2);
        let restore = BulkRestoreAction { column };
        assert_eq!(restore.run(&one, pool).await.unwrap().affected, 1);
        assert_eq!(restore.run(&all, pool).await.unwrap().affected, 1);
        assert_eq!(ops(pool, "restore").await, 2);
        assert_eq!(ops(pool, "update").await, 0);
        let deleted = BulkDeleteAction.run(&all, pool).await.unwrap();
        assert_eq!(deleted.affected, 2);
        assert_eq!(ops(pool, "delete").await, 2);
    }

    tri_dialect_test! {
        setup: setup,
        scenarios: [bulk_actions_are_audited],
    }

    /// A text PK not named `id` (#1817).
    mod text_pk {
        use super::*;

        #[derive(Model, Debug, Clone)]
        #[rustango(table = "bulk1817_code", app = "audit1794")]
        #[allow(dead_code)]
        pub struct Code {
            #[rustango(primary_key, max_length = 16)]
            pub code: String,
            #[rustango(soft_delete)]
            pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
        }

        fn keys(codes: &[&str]) -> PkSet {
            PkSet::parse(Code::SCHEMA, codes).unwrap()
        }

        /// MySQL casts a text column to a number against an integer key, so
        /// `0` matched every non-numeric code; string keys touch only their row.
        async fn text_pk_actions_touch_only_selected_rows(pool: &Pool) {
            for code in ["abc", "abd", "0"] {
                let row = Code {
                    code: code.into(),
                    deleted_at: None,
                };
                row.insert_pool(pool).await.expect("insert");
            }
            assert!(PkSet::new(Code::SCHEMA, [0_i64]).is_err());
            let column = "deleted_at";
            let soft = BulkSoftDeleteAction { column };
            let restore = BulkRestoreAction { column };
            assert_eq!(soft.run(&keys(&["0"]), pool).await.unwrap().affected, 1);
            let rows = Code::objects().fetch(pool).await.unwrap();
            let mut active: Vec<&str> = rows
                .iter()
                .filter(|c| c.deleted_at.is_none())
                .map(|c| c.code.as_str())
                .collect();
            active.sort_unstable();
            assert_eq!(active, ["abc", "abd"], "only `0` is soft-deleted");
            assert_eq!(restore.run(&keys(&["0"]), pool).await.unwrap().affected, 1);
            assert_eq!(soft.run(&keys(&["abc"]), pool).await.unwrap().affected, 1);
            let both = keys(&["abc", "abd"]);
            assert_eq!(soft.run(&both, pool).await.unwrap().affected, 1);
            let all = keys(&["abc", "abd", "0"]);
            assert_eq!(restore.run(&all, pool).await.unwrap().affected, 2);
            let deleted = BulkDeleteAction.run(&keys(&["abc"]), pool).await.unwrap();
            assert_eq!(deleted.affected, 1);
            let mut left: Vec<String> = Code::objects()
                .fetch(pool)
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.code)
                .collect();
            left.sort();
            assert_eq!(left, ["0", "abd"]);
        }

        /// A full `PkSet` stays under every bind cap in one unaudited
        /// `IN` list; one more key is refused.
        async fn pk_set_is_capped_under_the_bind_limit(pool: &Pool) {
            let codes: Vec<String> = (0..=PkSet::MAX_KEYS).map(|i| format!("k{i}")).collect();
            assert!(PkSet::parse(Code::SCHEMA, &codes).is_err());
            let full = PkSet::parse(Code::SCHEMA, &codes[1..]).unwrap();
            assert_eq!(BulkDeleteAction.run(&full, pool).await.unwrap().affected, 0);
        }

        tri_dialect_test! {
            model: Code,
            scenarios: [
                text_pk_actions_touch_only_selected_rows,
                pk_set_is_capped_under_the_bind_limit,
            ],
        }
    }
}

/// Admin edit and delete views: a no-op edit and a row someone else
/// marked since the read write no audit row (#1907, #1929).
mod admin_views {
    use super::*;
    use rustango::signals::admin as sig;

    #[derive(Model, Debug, Clone)]
    #[rustango(
        table = "audit1929_admin_doc",
        app = "audit1794",
        audit(track = "title, deleted_at"),
        admin(actions = "delete_selected, restore_selected")
    )]
    #[allow(dead_code)]
    pub struct AdminDoc {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 64)]
        pub title: String,
        #[rustango(soft_delete)]
        pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    const TABLE: &str = "audit1929_admin_doc";

    /// Admin signals are process-global.
    static SIGNALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    /// The stamp the racing receiver wrote.
    static RACED: std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>> =
        std::sync::Mutex::new(None);

    async fn setup(pool: &Pool) {
        rustango::testkit::matrix::fresh_table::<AdminDoc>(pool).await;
        audit::ensure_table_pool(pool).await.expect("audit table");
        AuditLog::delete_where("entity_table", TABLE, pool)
            .await
            .expect("clear audit rows");
    }

    async fn count(pool: &Pool, operation: &str) -> i64 {
        AuditLog::objects()
            .filter("entity_table", TABLE)
            .filter("operation", operation)
            .count(pool)
            .await
            .expect("count audit rows")
    }

    async fn seed(pool: &Pool) -> Vec<i64> {
        let mut pks = Vec::new();
        for title in ["a", "b"] {
            let mut doc = AdminDoc {
                id: Auto::default(),
                title: title.into(),
                deleted_at: None,
            };
            doc.insert_pool(pool).await.expect("insert");
            pks.push(*doc.id.get().expect("pk"));
        }
        pks
    }

    async fn post(pool: &Pool, uri: &str, form: String) -> StatusCode {
        let app = rustango::admin::Builder::new(pool.clone())
            .admin_prefix("")
            .build();
        send(app, Method::POST, uri, form, true).await
    }

    async fn stamp(pool: &Pool, pk: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        AdminDoc::objects()
            .filter("id", pk)
            .fetch(pool)
            .await
            .expect("fetch")
            .remove(0)
            .deleted_at
    }

    /// Soft-delete (or restore) `pk` from the admin pre-signal, after the
    /// view's read and before its write: the forced interleave.
    fn race(pool: &Pool, pk: i64, restore: bool) -> (sig::ReceiverId, sig::ReceiverId) {
        let target = pk.to_string();
        let run = move |pool: Pool, ctx_pk: String| {
            let target = target.clone();
            async move {
                if ctx_pk != target {
                    return;
                }
                let id: i64 = ctx_pk.parse().unwrap();
                let pk = SqlValue::I64(id);
                let n = if restore {
                    rustango::soft_delete::restore(&pool, AdminDoc::SCHEMA, "id", pk).await
                } else {
                    rustango::soft_delete::soft_delete(&pool, AdminDoc::SCHEMA, "id", pk).await
                };
                assert_eq!(n.unwrap(), 1);
                let raced = stamp(&pool, id).await;
                *RACED.lock().unwrap() = raced;
            }
        };
        let (p1, r1) = (pool.clone(), run.clone());
        let del = sig::connect_admin_pre_delete(move |c| r1(p1.clone(), c.pk));
        let (p2, r2) = (pool.clone(), run);
        let save = sig::connect_admin_pre_save(move |c| r2(p2.clone(), c.pk));
        (del, save)
    }

    fn unrace((del, save): (sig::ReceiverId, sig::ReceiverId)) {
        sig::disconnect_admin_pre_delete(del);
        sig::disconnect_admin_pre_save(save);
    }

    async fn noop_admin_edit_writes_no_update_row(pool: &Pool) {
        let pks = seed(pool).await;
        let uri = format!("/{TABLE}/{}", pks[0]);
        let status = post(pool, &uri, "title=a".into()).await;
        assert!(status.is_redirection(), "{status}");
        assert_eq!(count(pool, "update").await, 0, "a no-op edit was audited");
        post(pool, &uri, "title=z".into()).await;
        assert_eq!(count(pool, "update").await, 1);
    }

    /// A stale form that undoes a concurrent edit is audited: the diff
    /// reads the row the UPDATE overwrites, not the earlier read.
    async fn stale_admin_edit_is_audited(pool: &Pool) {
        let _g = SIGNALS.lock().await;
        let pks = seed(pool).await;
        let target = pks[0].to_string();
        let p = pool.clone();
        let id = sig::connect_admin_pre_save(move |c| {
            let (p, target) = (p.clone(), target.clone());
            async move {
                if c.pk != target {
                    return;
                }
                let q = rustango::core::UpdateQuery::new(
                    AdminDoc::SCHEMA,
                    vec![rustango::core::Assignment::new(
                        "title",
                        SqlValue::String("other".into()),
                    )],
                    rustango::core::WhereExpr::Predicate(rustango::core::Filter::new(
                        "id",
                        rustango::core::Op::Eq,
                        SqlValue::I64(c.pk.parse().unwrap()),
                    )),
                );
                rustango::sql::update_pool(&p, &q).await.expect("race");
            }
        });
        let status = post(pool, &format!("/{TABLE}/{}", pks[0]), "title=a".into()).await;
        sig::disconnect_admin_pre_save(id);
        assert!(status.is_redirection(), "{status}");
        let doc = AdminDoc::objects().filter("id", pks[0]).fetch(pool).await;
        assert_eq!(doc.unwrap()[0].title, "a");
        assert_eq!(count(pool, "update").await, 1, "the undo left no audit row");
    }

    async fn delete_view_keeps_a_concurrent_stamp(pool: &Pool) {
        let _g = SIGNALS.lock().await;
        let pks = seed(pool).await;
        let ids = race(pool, pks[0], false);
        let status = post(pool, &format!("/{TABLE}/{}/delete", pks[0]), String::new()).await;
        unrace(ids);
        assert!(status.is_redirection(), "{status}");
        let raced = *RACED.lock().unwrap();
        assert!(raced.is_some());
        assert_eq!(stamp(pool, pks[0]).await, raced, "re-stamped");
        assert_eq!(count(pool, "soft_delete").await, 1, "double audit");
    }

    async fn bulk_actions_skip_rows_marked_since_the_read(pool: &Pool) {
        let _g = SIGNALS.lock().await;
        let pks = seed(pool).await;
        let form =
            |action: &str| format!("action={action}&_selected={}&_selected={}", pks[0], pks[1]);
        let ids = race(pool, pks[0], false);
        let status = post(pool, &format!("/{TABLE}/__action"), form("delete_selected")).await;
        unrace(ids);
        assert!(status.is_redirection(), "{status}");
        assert_eq!(count(pool, "soft_delete").await, 2, "row 1 audited twice");
        let raced = *RACED.lock().unwrap();
        assert_eq!(stamp(pool, pks[0]).await, raced, "re-stamped");

        let ids = race(pool, pks[0], true);
        let status = post(
            pool,
            &format!("/{TABLE}/__action"),
            form("restore_selected"),
        )
        .await;
        unrace(ids);
        assert!(status.is_redirection(), "{status}");
        assert_eq!(
            count(pool, "restore").await,
            1,
            "the racing restore is missing"
        );
        assert_eq!(
            count(pool, "update").await,
            1,
            "the admin restore audited row 1"
        );
        assert!(stamp(pool, pks[0]).await.is_none());
    }

    /// POST a form to the admin; the status and the page body.
    async fn post_page(pool: &Pool, uri: &str, form: &str) -> (StatusCode, String) {
        let app = rustango::admin::Builder::new(pool.clone())
            .admin_prefix("")
            .build();
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
            .body(Body::from(format!("_csrf={CSRF}&{form}")))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Insert one row titled "a" through `schema`'s table; its PK.
    async fn seed_one(pool: &Pool, schema: &'static rustango::core::ModelSchema) -> i64 {
        let q = rustango::core::InsertQuery::new(schema, vec!["title"], vec!["a".into()]);
        rustango::sql::insert_pool(pool, &q).await.expect("seed");
        let rows = rustango::sql::select_rows_as_json(
            pool,
            &rustango::core::SelectQuery::new(schema),
            &schema.scalar_fields().collect::<Vec<_>>(),
        )
        .await
        .expect("read seed");
        rows[0]["id"].as_i64().expect("pk")
    }

    /// An edit whose audit row cannot be written is not saved (#2060).
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn admin_edit_rolls_back_when_its_audit_fails() {
        // No audit table here, so the emit fails.
        let pool = rustango::testkit::matrix::sqlite_file_pool().await;
        rustango::testkit::matrix::fresh_table::<AdminDoc>(&pool).await;
        let pk = seed_one(&pool, AdminDoc::SCHEMA).await;
        let uri = format!("/{TABLE}/{pk}");
        let (status, body) = post_page(&pool, &uri, "title=z").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("audit table missing — run `manage migrate`"),
            "{body}"
        );
        let title = || async {
            AdminDoc::objects().fetch(&pool).await.unwrap()[0]
                .title
                .clone()
        };
        assert_eq!(
            title().await,
            "a",
            "the edit committed without its audit row"
        );

        audit::ensure_table_pool(&pool).await.expect("audit table");
        let (status, body) = post_page(&pool, &uri, "title=z").await;
        assert!(status.is_redirection(), "{status} {body}");
        assert_eq!(title().await, "z");
    }

    /// A create whose audit row cannot be written is not saved (#2101).
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn admin_create_rolls_back_when_its_audit_fails() {
        // No audit table here, so the emit fails.
        let pool = rustango::testkit::matrix::sqlite_file_pool().await;
        rustango::testkit::matrix::fresh_table::<AdminDoc>(&pool).await;
        let uri = format!("/{TABLE}");
        let (status, body) = post_page(&pool, &uri, "title=z").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("audit table missing — run `manage migrate`"),
            "{body}"
        );
        let rows = || async { AdminDoc::objects().fetch(&pool).await.unwrap().len() };
        assert_eq!(
            rows().await,
            0,
            "the create committed without its audit row"
        );

        audit::ensure_table_pool(&pool).await.expect("audit table");
        let (status, body) = post_page(&pool, &uri, "title=z").await;
        assert!(status.is_redirection(), "{status} {body}");
        assert_eq!(rows().await, 1);
    }

    /// No `audit(...)`: the admin still logs edits, best-effort.
    #[derive(Model, Debug, Clone)]
    #[rustango(table = "audit2060_plain_doc", app = "audit1794")]
    #[allow(dead_code)]
    pub struct PlainDoc {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 64)]
        pub title: String,
    }

    /// A model without `audit(...)` is edited even with no audit table.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn unaudited_admin_edit_saves_without_the_audit_table() {
        let pool = rustango::testkit::matrix::sqlite_file_pool().await;
        rustango::testkit::matrix::fresh_table::<PlainDoc>(&pool).await;
        let pk = seed_one(&pool, PlainDoc::SCHEMA).await;
        let uri = format!("/audit2060_plain_doc/{pk}");
        let (status, body) = post_page(&pool, &uri, "title=z").await;
        assert!(status.is_redirection(), "{status} {body}");
        let docs = PlainDoc::objects().fetch(&pool).await.unwrap();
        assert_eq!(docs[0].title, "z");
    }

    tri_dialect_test! {
        setup: setup,
        scenarios: [
            noop_admin_edit_writes_no_update_row,
            stale_admin_edit_is_audited,
            delete_view_keeps_a_concurrent_stamp,
            bulk_actions_skip_rows_marked_since_the_read,
        ],
    }
}
