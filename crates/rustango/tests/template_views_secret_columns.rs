//! Template views keep credential columns out of the Tera context unless
//! a view lists them; UpdateView and the bulk-delete confirm page carry
//! only their field lists plus the primary key.

#![cfg(all(feature = "sqlite", feature = "template_views"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::template_views::{DetailView, ListView, UpdateView};
use rustango::testkit::matrix::{fresh_table, Backend};
use rustango::Model;
use tera::Tera;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "tvsecret_account", app = "tvsecret")]
pub struct Account {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    #[rustango(max_length = 64)]
    pub password_hash: String,
    #[rustango(max_length = 32)]
    pub note: String,
}

const CSRF: &str = "tvsecret-csrf-token-tvsecret-csrf-token-xxxx";

fn app(pool: &Pool) -> axum::Router {
    let mut t = Tera::default();
    t.add_raw_templates(vec![
        ("one.html", "{{ object | json_encode() | safe }}"),
        ("list.html", "list"),
        ("bulk.html", "{{ objects | json_encode() | safe }}"),
    ])
    .unwrap();
    let t = Arc::new(t);
    UpdateView::for_model(Account::SCHEMA, &["name"])
        .template("one.html")
        .router("/acc", t.clone(), pool.clone())
        .merge(
            UpdateView::for_model(Account::SCHEMA, &["name", "password_hash"])
                .template("one.html")
                .router("/listed", t.clone(), pool.clone()),
        )
        .merge(
            UpdateView::for_model(Account::SCHEMA, &["name"])
                .context_fields(&["note"])
                .template("one.html")
                .router("/ctx", t.clone(), pool.clone()),
        )
        .merge(
            DetailView::for_model(Account::SCHEMA)
                .template("one.html")
                .router("/acc", t.clone(), pool.clone()),
        )
        .merge(
            ListView::for_model(Account::SCHEMA)
                .template("list.html")
                .fields(&["id", "name"])
                .bulk_actions(true)
                .with_delete_confirmation(true)
                .with_delete_confirmation_template("bulk.html")
                .router("/acc", t, pool.clone()),
        )
}

async fn send(pool: &Pool, method: Method, uri: &str, body: &str) -> String {
    let resp = app(pool)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
                .body(Body::from(format!("_csrf={CSRF}&{body}")))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    assert!(status.is_success(), "{uri}: {status}: {body}");
    body
}

#[tokio::test]
async fn credential_columns_stay_out_of_the_context() {
    let pool = Backend::Sqlite.pool().await.expect("sqlite");
    fresh_table::<Account>(&pool).await;
    let mut row = Account {
        id: Auto::default(),
        name: "ada".into(),
        password_hash: "argon2-SECRET".into(),
        note: "UNLISTED".into(),
    };
    row.insert_pool(&pool).await.expect("seed");
    let pk = *row.id.get().unwrap();

    let edit = send(&pool, Method::GET, &format!("/acc/{pk}/edit"), "").await;
    let confirm = send(
        &pool,
        Method::POST,
        "/acc",
        &format!("action=delete_selected&_selected_action={pk}"),
    )
    .await;
    let detail = send(&pool, Method::GET, &format!("/acc/{pk}"), "").await;
    for (uri, out) in [("edit", &edit), ("confirm", &confirm), ("detail", &detail)] {
        assert!(!out.contains("SECRET"), "{uri} leaks the hash: {out}");
        assert!(out.contains("ada"), "{uri}: {out}");
        assert!(
            out.contains(&format!("\"id\":{pk}")),
            "{uri} lacks the pk: {out}"
        );
    }
    // UpdateView and the confirm page carry only their field lists and the pk.
    for (uri, out) in [("edit", &edit), ("confirm", &confirm)] {
        assert!(
            !out.contains("UNLISTED"),
            "{uri} leaks an unlisted column: {out}"
        );
        assert!(!out.contains("\"note\""), "{uri}: {out}");
    }
    // DetailView without `.fields()` still shows every non-secret column.
    assert!(detail.contains("UNLISTED"), "{detail}");
    // A view that lists the column on purpose still gets it.
    let out = send(&pool, Method::GET, &format!("/listed/{pk}/edit"), "").await;
    assert!(out.contains("SECRET"), "{out}");
    // `.context_fields()` opts a read-only column in.
    let out = send(&pool, Method::GET, &format!("/ctx/{pk}/edit"), "").await;
    assert!(out.contains("UNLISTED") && !out.contains("SECRET"), "{out}");
}
