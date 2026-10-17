//! Admin edit forms on every backend: an untouched save keeps what was
//! stored (#1273).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "aed_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub meta: Option<serde_json::Value>,
    #[rustango(max_length = 16, choices = "draft:Draft, live:Live")]
    pub status: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
}

async fn send(pool: &Pool, method: &str, uri: &str, form: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// What a browser submits for control `name` of the rendered form.
fn shown(html: &str, name: &str) -> String {
    let open = format!(r#"name="{name}""#);
    let at = html.find(&open).unwrap_or_else(|| panic!("no `{name}` control"));
    let tag_start = html[..at].rfind('<').unwrap();
    let rest = &html[tag_start..];
    if rest.starts_with("<textarea") {
        let body = &rest[rest.find('>').unwrap() + 1..];
        return unescape(&body[..body.find("</textarea>").unwrap()]);
    }
    if rest.starts_with("<select") {
        let select = &rest[..rest.find("</select>").unwrap()];
        let options: Vec<&str> = select.split("<option").skip(1).collect();
        let pick = options
            .iter()
            .find(|o| o[..o.find('>').unwrap()].contains(" selected"))
            .or(options.first())
            .expect("an option");
        let v = &pick[pick.find("value=\"").unwrap() + 7..];
        return unescape(&v[..v.find('"').unwrap()]);
    }
    let tag = &rest[..rest.find('>').unwrap()];
    let v = &tag[tag.find("value=\"").unwrap() + 7..];
    unescape(&v[..v.find('"').unwrap()])
}

fn encode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A nullable `{}` and a value outside `choices` survive a save that
/// only renames the row (#1273).
async fn untouched_json_and_unknown_choice_are_kept(pool: &Pool) {
    let mut d = Doc {
        id: Auto::default(),
        name: "a".into(),
        meta: Some(serde_json::json!({})),
        status: "legacy".into(),
    };
    d.insert_pool(pool).await.expect("insert");
    let id = *d.id.get().expect("pk");
    let (status, html) = send(pool, "GET", &format!("/aed_doc/{id}"), "").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let form = encode(&[
        ("name", "renamed".to_owned()),
        ("meta", shown(&html, "meta")),
        ("status", shown(&html, "status")),
    ]);
    let (status, body) = send(pool, "POST", &format!("/aed_doc/{id}"), &form).await;
    assert!(status.is_redirection(), "{status}: {body}");
    let row = Doc::objects().fetch(pool).await.expect("fetch").remove(0);
    assert_eq!(row.name, "renamed");
    assert_eq!(row.meta, Some(serde_json::json!({})), "nullable {{}} lost");
    assert_eq!(row.status, "legacy", "unknown choice overwritten");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        untouched_json_and_unknown_choice_are_kept,
    ],
}

#[cfg(feature = "postgres")]
mod pg_only {
    use super::*;
    use rustango::sql::{Array, HStore, Range, Vector};

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "aed_pgkinds")]
    #[allow(dead_code)]
    pub struct PgKinds {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 32)]
        pub name: String,
        pub tags: Array<String>,
        pub seats: Range<i32>,
        pub attrs: HStore,
        #[rustango(vector(dims = 3))]
        pub emb: Vector,
        #[rustango(vector(dims = 3))]
        pub opt_emb: Option<Vector>,
    }

    /// Array, range, hstore and vector columns show their value and
    /// survive an untouched save (#1273).
    #[tokio::test]
    async fn untouched_pg_types_are_kept() {
        use rustango::testkit::matrix::{live_lock, Backend};
        let _guard = live_lock().lock().await;
        let Some(pool) = Backend::Postgres.pool().await else {
            eprintln!("DATABASE_URL not set — skipping");
            return;
        };
        for ext in ["hstore", "vector"] {
            rustango::sql::raw_execute_pool(
                &pool,
                &format!("CREATE EXTENSION IF NOT EXISTS {ext}"),
                Vec::new(),
            )
            .await
            .expect("extension");
        }
        rustango::testkit::matrix::fresh_table::<PgKinds>(&pool).await;
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("k".to_owned(), Some("v".to_owned()));
        let mut row = PgKinds {
            id: Auto::default(),
            name: "a".into(),
            tags: Array(vec!["x".into(), "y".into()]),
            seats: Range::closed_open(1, 10),
            attrs: HStore(attrs.clone()),
            emb: Vector::new(vec![1.0, 2.0, 3.0]),
            opt_emb: Some(Vector::new(vec![0.5, 0.0, 0.0])),
        };
        row.insert_pool(&pool).await.expect("insert");
        let id = *row.id.get().expect("pk");
        let (status, html) = send(&pool, "GET", &format!("/aed_pgkinds/{id}"), "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let mut pairs = vec![("name", "renamed".to_owned())];
        for f in ["tags", "seats", "attrs", "emb", "opt_emb"] {
            pairs.push((f, shown(&html, f)));
        }
        assert_eq!(pairs[1].1, "x, y", "array prefill");
        let (status, body) =
            send(&pool, "POST", &format!("/aed_pgkinds/{id}"), &encode(&pairs)).await;
        assert!(status.is_redirection(), "{status}: {body}");
        let got = PgKinds::objects().fetch(&pool).await.expect("fetch").remove(0);
        assert_eq!(got.name, "renamed");
        assert_eq!(got.tags.0, ["x", "y"]);
        assert_eq!(got.seats, Range::closed_open(1, 10));
        assert_eq!(got.attrs.0, attrs);
        assert_eq!(got.emb, Vector::new(vec![1.0, 2.0, 3.0]));
        assert_eq!(got.opt_emb, Some(Vector::new(vec![0.5, 0.0, 0.0])));
    }
}
