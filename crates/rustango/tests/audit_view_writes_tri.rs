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
use rustango::sql::{Auto, CounterPool as _, Pool};
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
    assert_eq!(ops(pool, "delete").await, 1);
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

tri_dialect_test! {
    setup: setup,
    scenarios: [
        viewset_update_and_delete_are_audited,
        template_views_writes_are_audited,
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
        use rustango::sql::FetcherPool as _;

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
            assert_eq!(soft.run(&keys(&["abc"]), pool).await.unwrap().affected, 1);
            let both = keys(&["abc", "abd"]);
            assert_eq!(soft.run(&both, pool).await.unwrap().affected, 1);
            let restore = BulkRestoreAction { column };
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

        tri_dialect_test! {
            model: Code,
            scenarios: [text_pk_actions_touch_only_selected_rows],
        }
    }
}
